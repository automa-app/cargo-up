use crate::{
    ra_ap_syntax::{ast, AstNode},
    semver::{SemVerError, Version as SemverVersion},
    utils::{normalize, Error, INTERNAL_ERR, TERM_ERR, TERM_OUT},
    Preloader, Semantics, Upgrader, Version, Visitor,
};
use ra_ap_base_db::{FileId, SourceDatabaseExt};
use ra_ap_hir::{AssocItem, Crate, ModuleDef, PathResolution};
use ra_ap_ide_db::symbol_index::SymbolsDatabase;
use ra_ap_rust_analyzer::cli::load_cargo;
use ra_ap_text_edit::TextEdit;
use std::{
    collections::BTreeMap as Map,
    fs::{read_to_string, write},
    io::Result as IoResult,
    path::Path,
};

pub struct Runner {
    pub(crate) minimum: Option<SemverVersion>,
    pub(crate) versions: Vec<Version>,
    upgrader: Upgrader,
    version: SemverVersion,
    preloader: Preloader,
    current_file: Option<FileId>,
}

impl Runner {
    pub fn new() -> Self {
        Self {
            minimum: None,
            versions: vec![],
            upgrader: Upgrader::default(),
            version: SemverVersion::parse("0.0.0").expect(INTERNAL_ERR),
            preloader: Preloader::default(),
            current_file: None,
        }
    }

    pub fn minimum(mut self, version: &str) -> Result<Self, SemVerError> {
        self.minimum = Some(SemverVersion::parse(version)?);
        Ok(self)
    }

    pub fn version(mut self, version: Version) -> Self {
        self.versions.push(version);
        self
    }
}

impl Runner {
    #[doc(hidden)]
    pub fn run(
        &mut self,
        root: &Path,
        dep: &str,
        from: SemverVersion,
        to: SemverVersion,
    ) -> IoResult<()> {
        let (host, vfs) = load_cargo(root, true, false).unwrap();
        let db = host.raw_database();

        let mut changes = Map::<FileId, TextEdit>::new();
        let semantics = Semantics::new(db);

        if let Some(min) = self.minimum.clone() {
            if from < min {
                return Error::NotMinimum(dep.into(), min.to_string()).print_err();
            }
        }

        self.version = to;
        let version = self.get_version();

        let peers = if let Some(version) = version {
            let mut peers = vec![dep.to_string()];
            peers.extend(version.peers.clone());
            peers
        } else {
            return Error::NoChanges(self.version.to_string()).print_out();
        };

        // Loop to find and eager load the dep we are upgrading
        for krate in Crate::all(db) {
            let file_id = krate.root_file(db);
            let source_root = db.source_root(db.file_source_root(file_id));

            if !source_root.is_library {
                continue;
            }

            if let Some(name) = krate.display_name(db) {
                if let Some(peer) = peers
                    .iter()
                    .find(|x| **x == normalize(&format!("{}", name)))
                {
                    self.preloader.load(peer, db, &krate);
                }
            }
        }

        // Actual loop to walk through the source code
        for source_root_id in db.local_roots().iter() {
            let source_root = db.source_root(*source_root_id);

            for file_id in source_root.iter() {
                self.current_file = Some(file_id);
                let source_file = semantics.parse(file_id);
                // println!("{:#?}", source_file.syntax());

                self.visit(source_file.syntax(), &semantics);

                changes.insert(file_id, self.upgrader.finish());
            }
        }

        // Apply changes
        for (file_id, edit) in changes {
            let full_path = vfs.file_path(file_id);
            let full_path = full_path.as_path().expect(INTERNAL_ERR);

            let mut file_text = read_to_string(&full_path)?;

            edit.apply(&mut file_text);
            write(&full_path, file_text)?;
        }

        TERM_ERR.flush()?;
        TERM_OUT.flush()?;

        Ok(())
    }

    fn get_version(&self) -> Option<&Version> {
        self.versions.iter().find(|x| x.version == self.version)
    }
}

impl Runner {
    fn check_name_or_name_ref(
        name_or_name_ref: Option<ast::NameOrNameRef>,
        map: &Map<String, Map<String, String>>,
    ) -> Option<bool> {
        let name = match name_or_name_ref? {
            ast::NameOrNameRef::NameRef(name_ref) => name_ref.text().to_string(),
            ast::NameOrNameRef::Name(name) => name.text().to_string(),
        };

        Some(map.iter().any(|x| x.1.iter().any(|y| *y.0 == name)))
    }

    fn check_name_ref(
        name_ref: Option<ast::NameRef>,
        map: &Map<String, Map<String, String>>,
    ) -> Option<bool> {
        Self::check_name_or_name_ref(name_ref.map(|x| ast::NameOrNameRef::NameRef(x)), map)
    }

    fn check_path(path: Option<ast::Path>, map: &Map<String, Map<String, String>>) -> Option<bool> {
        Self::check_name_ref(path?.segment()?.name_ref(), map)
    }
}

impl Visitor for Runner {
    fn visit_source_file(&mut self, _: &ast::SourceFile, _: &Semantics) {}

    fn visit_method_call_expr(
        &mut self,
        method_call_expr: &ast::MethodCallExpr,
        semantics: &Semantics,
    ) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_method_call_expr {
            hook(&mut upgrader, method_call_expr, semantics);
        }

        if let Some(true) =
            Self::check_name_ref(method_call_expr.name_ref(), &version.rename_methods)
        {
            if let Some(f) = semantics.resolve_method_call(method_call_expr) {
                if let Some((_, name)) = self.preloader.methods.iter().find(|x| *x.0 == f) {
                    if let Some(map) = version.rename_methods.get(name) {
                        let name_ref = method_call_expr.name_ref().expect(INTERNAL_ERR);
                        let method_name = name_ref.text().to_string();

                        if let Some(to) = map.get(&method_name) {
                            upgrader.replace(name_ref.syntax().text_range(), to.to_string());
                        }
                    }
                }
            }
        }

        self.upgrader = upgrader;
    }

    fn visit_call_expr(&mut self, call_expr: &ast::CallExpr, semantics: &Semantics) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_call_expr {
            hook(&mut upgrader, call_expr, semantics);
        }

        self.upgrader = upgrader;
    }

    fn visit_path_expr(&mut self, path_expr: &ast::PathExpr, semantics: &Semantics) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_path_expr {
            hook(&mut upgrader, path_expr, semantics);
        }

        if let Some(true) = Self::check_path(path_expr.path(), &version.rename_variants) {
            let path = path_expr.path().expect(INTERNAL_ERR);

            if let Some(PathResolution::Def(ModuleDef::EnumVariant(e))) =
                semantics.resolve_path(&path)
            {
                if let Some((_, name)) = self.preloader.variants.iter().find(|x| *x.0 == e) {
                    if let Some(map) = version.rename_variants.get(name) {
                        let name_ref = path
                            .segment()
                            .expect(INTERNAL_ERR)
                            .name_ref()
                            .expect(INTERNAL_ERR);
                        let variant_name = name_ref.text().to_string();

                        if let Some(to) = map.get(&variant_name) {
                            upgrader.replace(name_ref.syntax().text_range(), to.to_string());
                        }
                    }
                }
            }
        } else if let Some(true) = Self::check_path(path_expr.path(), &version.rename_methods) {
            let path = path_expr.path().expect(INTERNAL_ERR);

            if let Some(PathResolution::AssocItem(AssocItem::Function(f))) =
                semantics.resolve_path(&path)
            {
                if let Some((_, name)) = self.preloader.methods.iter().find(|x| *x.0 == f) {
                    if let Some(map) = version.rename_methods.get(name) {
                        let name_ref = path
                            .segment()
                            .expect(INTERNAL_ERR)
                            .name_ref()
                            .expect(INTERNAL_ERR);
                        let method_name = name_ref.text().to_string();

                        if let Some(to) = map.get(&method_name) {
                            upgrader.replace(name_ref.syntax().text_range(), to.to_string());
                        }
                    }
                }
            }
        }

        self.upgrader = upgrader;
    }

    fn visit_field_expr(&mut self, field_expr: &ast::FieldExpr, semantics: &Semantics) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_field_expr {
            hook(&mut upgrader, field_expr, semantics);
        }

        if let Some(true) = Self::check_name_ref(field_expr.name_ref(), &version.rename_members) {
            if let Some(f) = semantics.resolve_field(field_expr) {
                if let Some((_, name)) = self.preloader.members.iter().find(|x| *x.0 == f) {
                    if let Some(map) = version.rename_members.get(name) {
                        let name_ref = field_expr.name_ref().expect(INTERNAL_ERR);
                        let member_name = name_ref.text().to_string();

                        if let Some(to) = map.get(&member_name) {
                            upgrader.replace(name_ref.syntax().text_range(), to.to_string());
                        }
                    }
                }
            }
        }

        self.upgrader = upgrader;
    }

    fn visit_record_pat(&mut self, record_pat: &ast::RecordPat, semantics: &Semantics) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_record_pat {
            hook(&mut upgrader, record_pat, semantics);
        }

        self.upgrader = upgrader;
    }

    fn visit_record_expr(&mut self, record_expr: &ast::RecordExpr, semantics: &Semantics) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_record_expr {
            hook(&mut upgrader, record_expr, semantics);
        }

        self.upgrader = upgrader;
    }

    fn visit_record_expr_field(
        &mut self,
        record_expr_field: &ast::RecordExprField,
        semantics: &Semantics,
    ) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_record_expr_field {
            hook(&mut upgrader, record_expr_field, semantics);
        }

        if let Some(true) =
            Self::check_name_ref(record_expr_field.field_name(), &version.rename_members)
        {
            if let Some(f) = semantics.resolve_record_field(record_expr_field) {
                if let Some((_, name)) = self.preloader.members.iter().find(|x| *x.0 == f.0) {
                    if let Some(map) = version.rename_members.get(name) {
                        if let Some(name_ref) = record_expr_field.name_ref() {
                            let member_name = name_ref.text().to_string();

                            if let Some(to) = map.get(&member_name) {
                                upgrader.replace(name_ref.syntax().text_range(), to.to_string());
                            }
                        } else if let Some(ast::Expr::PathExpr(path_expr)) =
                            record_expr_field.expr()
                        {
                            let member_name = path_expr
                                .path()
                                .expect(INTERNAL_ERR)
                                .segment()
                                .expect(INTERNAL_ERR)
                                .name_ref()
                                .expect(INTERNAL_ERR)
                                .text()
                                .to_string();

                            if let Some(to) = map.get(&member_name) {
                                upgrader.replace(
                                    path_expr.syntax().text_range(),
                                    format!("{}: {}", to, member_name),
                                );
                            }
                        }
                    }
                }
            }
        }

        self.upgrader = upgrader;
    }

    fn visit_record_pat_field(
        &mut self,
        record_pat_field: &ast::RecordPatField,
        semantics: &Semantics,
    ) {
        let mut upgrader = self.upgrader.clone();
        let version = self.get_version().expect(INTERNAL_ERR);

        for hook in &version.hook_record_pat_field {
            hook(&mut upgrader, record_pat_field, semantics);
        }

        if let Some(true) =
            Self::check_name_or_name_ref(record_pat_field.field_name(), &version.rename_members)
        {
            if let Some(f) = semantics.resolve_record_field_pat(record_pat_field) {
                if let Some((_, name)) = self.preloader.members.iter().find(|x| *x.0 == f) {
                    if let Some(map) = version.rename_members.get(name) {
                        match record_pat_field.field_name() {
                            Some(ast::NameOrNameRef::Name(name)) => {
                                let member_name = name.text().to_string();

                                if let Some(to) = map.get(&member_name) {
                                    upgrader.replace(
                                        record_pat_field.syntax().text_range(),
                                        format!("{}: {}", to, record_pat_field),
                                    );
                                }
                            }
                            Some(ast::NameOrNameRef::NameRef(name_ref)) => {
                                let member_name = name_ref.text().to_string();

                                if let Some(to) = map.get(&member_name) {
                                    upgrader
                                        .replace(name_ref.syntax().text_range(), to.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        self.upgrader = upgrader;
    }
}
