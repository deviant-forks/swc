use crate::{
    bundler::{
        chunk::merge::Ctx,
        load::{Source, Specifier, TransformedModule},
    },
    util::{ExprExt, MapWithMut, VarDeclaratorExt},
    Bundler, Load, ModuleId, Resolve,
};
use anyhow::{Context, Error};
#[cfg(feature = "concurrent")]
use rayon::iter::ParallelIterator;
use std::mem::{replace, take};
use swc_common::{Spanned, SyntaxContext, DUMMY_SP};
use swc_ecma_ast::*;
use swc_ecma_utils::{find_ids, ident::IdentLike, Id};
use swc_ecma_visit::{noop_fold_type, noop_visit_mut_type, Fold, FoldWith, VisitMut, VisitMutWith};

impl<L, R> Bundler<'_, L, R>
where
    L: Load,
    R: Resolve,
{
    /// This methods injects varaibles to connect two modules.
    ///
    /// (_n) denotes hygiene. Actual name is `bar`, not `bar_1`.
    ///
    /// # Entry
    ///
    /// ```js
    /// import { bar_1, baz_1 } from './a';
    /// console.log(bar_1, baz_1);
    /// ```
    ///
    /// # Dep
    ///
    /// ```js
    /// const foo_2 = 1;
    /// const bar_2 = 2;
    /// export { foo_2 as bar_2 };
    /// export { bar_2 as baz_2 };     
    /// ```
    ///
    /// # Output
    ///
    /// ```js
    /// const foo_2 = 1;
    /// const bar_2 = 2;
    ///
    /// const bar_1 = foo_2;
    /// const baz_1 = bar_2;
    ///
    /// console.log(bar, baz);
    /// ```
    pub(super) fn merge_export(
        &self,
        ctx: &Ctx,
        dep_id: ModuleId,
        specifiers: &[Specifier],
    ) -> Result<Module, Error> {
        self.run(|| {
            log::debug!("Reexporting {:?}", dep_id);
            let dep_info = self.scope.get_module(dep_id).unwrap();
            let mut dep = self
                .merge_modules(ctx, dep_id, false, true)
                .context("failed to get module for merging")?;

            // print_hygiene(
            //     &format!(
            //         "reexport: load dep: {} ({:?}, {:?})",
            //         dep_info.fm.name,
            //         dep_info.local_ctxt(),
            //         dep_info.export_ctxt()
            //     ),
            //     &self.cm,
            //     &dep,
            // );

            self.handle_reexport(&dep_info, &mut dep);

            // print_hygiene(&format!("dep: handle reexport"), &self.cm, &dep);

            // for stmt in &mut dep.body {
            //     let decl = match stmt {
            //         ModuleItem::ModuleDecl(decl) => decl,
            //         ModuleItem::Stmt(_) => continue,
            //     };

            //     match decl {
            //         ModuleDecl::ExportDecl(_) => {}
            //         ModuleDecl::ExportNamed(export) => {
            //             for specifier in &mut export.specifiers {
            //                 match specifier {
            //                     ExportSpecifier::Namespace(ns) => {}
            //                     ExportSpecifier::Default(default) => {}
            //                     ExportSpecifier::Named(named) => match &mut
            // named.exported {                         Some(exported) => {
            //                             if exported.span.ctxt != dep_info.local_ctxt() {
            //                                 continue;
            //                             }

            //                             exported.span =
            //
            // exported.span.with_ctxt(dep_info.export_ctxt());
            // }                         None => {
            //                             if named.orig.span.ctxt != dep_info.local_ctxt()
            // {                                 continue;
            //                             }

            //                             named.exported = Some(Ident::new(
            //                                 named.orig.sym.clone(),
            //
            // named.orig.span.with_ctxt(dep_info.export_ctxt()),
            // ));                         }
            //                     },
            //                 }
            //             }
            //         }
            //         ModuleDecl::ExportDefaultDecl(_) => {}
            //         ModuleDecl::ExportDefaultExpr(_) => {}
            //         ModuleDecl::ExportAll(_) => {}
            //         _ => {}
            //     }
            // }

            if let Some(module_name) = self.scope.wrapped_esm_id(dep_info.id) {
                dep = self.wrap_esm(ctx, dep_info.id, dep)?;

                for specifier in specifiers {
                    match specifier {
                        Specifier::Namespace { local, .. } => {
                            dep.body.push(
                                module_name
                                    .assign_to(local.clone())
                                    .into_module_item("merge_export"),
                            );
                            break;
                        }
                        _ => {}
                    }
                }
            }

            if !specifiers.is_empty() {
                dep.visit_mut_with(&mut UnexportAsVar {
                    dep_export_ctxt: dep_info.export_ctxt(),
                    _specifiers: &specifiers,
                });

                // print_hygiene(&format!("dep: unexport as var"), &self.cm, &dep);

                dep = dep.fold_with(&mut DepUnexporter {
                    exports: &specifiers,
                });

                // print_hygiene(&format!("dep: unexport"), &self.cm, &dep);
            }

            // TODO: Add varaible based on specifers

            Ok(dep)
        })
    }

    /// # ExportDecl
    ///
    /// For exported declarations, We should inject named exports.
    ///
    /// ```ts
    /// export const b__9 = 1;
    /// console.log(b__9);
    /// ```
    ///
    /// ```ts
    /// const b__9 = 1;
    /// export { b__9 as b__10 };
    /// console.log(b__9);
    /// ```
    fn handle_reexport(&self, info: &TransformedModule, module: &mut Module) {
        let mut new_body = Vec::with_capacity(module.body.len() + 20);

        for stmt in &mut module.body {
            let mut vars = vec![];
            let mut stmt = stmt.take();

            match &mut stmt {
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    for specifier in &import.specifiers {
                        match specifier {
                            ImportSpecifier::Named(named) => match &named.imported {
                                Some(imported) => {
                                    vars.push(imported.clone().assign_to(named.local.clone()));
                                }
                                None => {}
                            },
                            _ => {}
                        }
                    }
                    import.specifiers.clear();
                }

                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                    match &export.decl {
                        Decl::Class(ClassDecl { ident, .. }) | Decl::Fn(FnDecl { ident, .. }) => {
                            let mut exported = ident.clone();
                            exported.span.ctxt = info.export_ctxt();

                            vars.push(ident.clone().assign_to(exported));
                        }
                        Decl::Var(var) => {
                            //
                            let ids: Vec<Ident> = find_ids(&var.decls);

                            vars.extend(
                                ids.into_iter()
                                    .map(|i| {
                                        let mut exported = i.clone();
                                        exported.span.ctxt = info.export_ctxt();

                                        i.assign_to(exported)
                                    })
                                    .map(From::from),
                            );
                        }
                        _ => {}
                    };
                }

                _ => {}
            }

            new_body.push(stmt);
            for var in vars {
                new_body.push(var.into_module_item("from_export_rs"))
            }
        }

        module.body = new_body;
    }
}

pub(super) struct ExportInjector {
    pub imported: Vec<ModuleItem>,
    pub source: Source,
}

impl VisitMut for ExportInjector {
    noop_visit_mut_type!();

    fn visit_mut_module_items(&mut self, orig: &mut Vec<ModuleItem>) {
        let items = take(orig);
        let mut buf = Vec::with_capacity(self.imported.len() + items.len());

        for item in items {
            //
            match item {
                ModuleItem::Stmt(Stmt::Empty(..)) => continue,

                // If the case of importing and exporting from same module, we firstly
                // handle it using export.rs
                //
                // This works for the most time, but if the import has an alias, export
                // handler cannot handle this. So we inject some variables to connnect them.
                //
                // See: https://github.com/swc-project/swc/issues/1150
                ModuleItem::ModuleDecl(ModuleDecl::Import(ref import))
                    if import.src.value == self.source.src.value =>
                {
                    buf.extend(take(&mut self.imported));

                    let decls = import
                        .specifiers
                        .iter()
                        .filter_map(|specifier| match specifier {
                            ImportSpecifier::Named(ImportNamedSpecifier {
                                local,
                                imported: Some(imported),
                                ..
                            }) => {
                                let mut imported = imported.clone();
                                imported.span = imported.span.with_ctxt(self.source.export_ctxt);

                                Some(VarDeclarator {
                                    span: DUMMY_SP,
                                    name: Pat::Ident(local.clone()),
                                    init: Some(Box::new(Expr::Ident(imported))),
                                    definite: false,
                                })
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();

                    for var in decls {
                        buf.push(var.into_module_item("ExportInjector"));
                    }
                }

                ModuleItem::ModuleDecl(ModuleDecl::ExportAll(ref export))
                    if export.src.value == self.source.src.value =>
                {
                    buf.extend(take(&mut self.imported));
                }

                ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(
                    export @ NamedExport { src: Some(..), .. },
                )) if export.src.as_ref().unwrap().value == self.source.src.value => {
                    let namespace_name = export
                        .specifiers
                        .iter()
                        .filter_map(|specifier| match specifier {
                            ExportSpecifier::Namespace(ns) => Some(ns.name.clone()),
                            ExportSpecifier::Default(_) => None,
                            ExportSpecifier::Named(_) => None,
                        })
                        .next();

                    buf.extend(take(&mut self.imported));

                    if let Some(ns_name) = namespace_name {
                        buf.push(ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(
                            NamedExport {
                                span: export.span,
                                src: None,
                                specifiers: vec![ExportSpecifier::Named(ExportNamedSpecifier {
                                    span: DUMMY_SP,
                                    orig: ns_name,
                                    exported: None,
                                })],
                                type_only: false,
                            },
                        )));
                    } else {
                        buf.push(ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(
                            NamedExport {
                                src: None,
                                ..export
                            },
                        )));
                    }
                }

                _ => buf.push(item),
            }
        }

        *orig = buf;
    }
}

/// Converts
///
/// ```js
/// export { l1 as e2 };
/// ```
///
/// to
///
/// ```js
/// const e3 = l1;
/// ```
///
/// export { foo#7 } from './b' where #7 is mark of './b'
/// =>
/// export { foo#7 as foo#5 } where #5 is mark of current entry.
struct UnexportAsVar<'a> {
    /// Syntax context for the generated variables.
    dep_export_ctxt: SyntaxContext,

    /// Exports to preserve
    _specifiers: &'a [Specifier],
}

impl VisitMut for UnexportAsVar<'_> {
    noop_visit_mut_type!();

    fn visit_mut_module_item(&mut self, n: &mut ModuleItem) {
        n.visit_mut_children_with(self);

        match n {
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(export)) => {
                let expr = replace(
                    &mut export.expr,
                    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                );

                *n = VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(Ident::new(
                        "__default".into(),
                        expr.span().with_ctxt(self.dep_export_ctxt),
                    )),
                    init: Some(expr),
                    definite: false,
                }
                .into_module_item("UnexportAsVar");
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(
                ref export @ NamedExport { src: None, .. },
            )) => {
                let mut decls = vec![];
                for s in &export.specifiers {
                    match s {
                        ExportSpecifier::Namespace(_) => {}
                        ExportSpecifier::Default(_) => {}
                        ExportSpecifier::Named(n) => match &n.exported {
                            Some(exported) => {
                                // TODO: (maybe) Check previous context
                                let exported = exported.clone();
                                // exported.span = exported.span.with_ctxt(self.dep_ctxt);

                                if exported.sym != n.orig.sym
                                    || exported.span.ctxt != n.orig.span.ctxt
                                {
                                    decls.push(VarDeclarator {
                                        span: n.span,
                                        name: Pat::Ident(exported),
                                        init: Some(Box::new(Expr::Ident(n.orig.clone()))),
                                        definite: true,
                                    })
                                }
                            }
                            None => {
                                if n.orig.span.ctxt != self.dep_export_ctxt {
                                    log::trace!(
                                        "Alias: {:?} -> {:?}",
                                        n.orig,
                                        self.dep_export_ctxt
                                    );

                                    decls.push(VarDeclarator {
                                        span: n.span,
                                        name: Pat::Ident(Ident::new(
                                            n.orig.sym.clone(),
                                            n.orig.span.with_ctxt(self.dep_export_ctxt),
                                        )),
                                        init: Some(Box::new(Expr::Ident(n.orig.clone()))),
                                        definite: false,
                                    })
                                }
                            }
                        },
                    }
                }

                if decls.is_empty() {
                    *n = ModuleItem::Stmt(Stmt::Empty(EmptyStmt { span: DUMMY_SP }))
                } else {
                    *n = ModuleItem::Stmt(Stmt::Decl(Decl::Var(VarDecl {
                        span: export.span,
                        decls,
                        declare: false,
                        kind: VarDeclKind::Const,
                    })))
                }
            }
            _ => {}
        }
    }

    fn visit_mut_stmt(&mut self, _: &mut Stmt) {}
}

struct DepUnexporter<'a> {
    exports: &'a [Specifier],
}

impl DepUnexporter<'_> {
    fn is_exported(&self, id: &Id) -> bool {
        if self.exports.is_empty() {
            return true;
        }

        self.exports.iter().any(|s| match s {
            Specifier::Specific { local, .. } => local.to_id() == *id,
            Specifier::Namespace { local, all } => local.to_id() == *id || *all,
        })
    }
}

impl Fold for DepUnexporter<'_> {
    noop_fold_type!();

    fn fold_module_item(&mut self, item: ModuleItem) -> ModuleItem {
        match item {
            ModuleItem::ModuleDecl(decl) => match decl {
                ModuleDecl::ExportDecl(mut export) => {
                    match &mut export.decl {
                        Decl::Class(c) => {
                            if self.is_exported(&c.ident.to_id()) {
                                return ModuleItem::Stmt(Stmt::Decl(export.decl));
                            }
                        }
                        Decl::Fn(f) => {
                            if self.is_exported(&f.ident.to_id()) {
                                return ModuleItem::Stmt(Stmt::Decl(export.decl));
                            }
                        }
                        Decl::Var(..) => {
                            if self.exports.is_empty() {
                                return ModuleItem::Stmt(Stmt::Decl(export.decl));
                            }
                        }
                        _ => {}
                    }
                    ModuleItem::Stmt(Stmt::Decl(export.decl))
                }

                ModuleDecl::ExportDefaultDecl(export) => match export.decl {
                    DefaultDecl::Class(ClassExpr { ident: None, .. })
                    | DefaultDecl::Fn(FnExpr { ident: None, .. }) => {
                        ModuleItem::Stmt(Stmt::Empty(EmptyStmt { span: DUMMY_SP }))
                    }
                    DefaultDecl::TsInterfaceDecl(decl) => {
                        ModuleItem::Stmt(Stmt::Decl(Decl::TsInterface(decl)))
                    }

                    DefaultDecl::Class(ClassExpr {
                        ident: Some(ident),
                        class,
                    }) => ModuleItem::Stmt(Stmt::Decl(Decl::Class(ClassDecl {
                        declare: false,
                        ident,
                        class,
                    }))),

                    DefaultDecl::Fn(FnExpr {
                        ident: Some(ident),
                        function,
                    }) => ModuleItem::Stmt(Stmt::Decl(Decl::Fn(FnDecl {
                        declare: false,
                        function,
                        ident,
                    }))),
                },

                // Empty statement
                ModuleDecl::ExportAll(..)
                | ModuleDecl::ExportDefaultExpr(..)
                | ModuleDecl::ExportNamed(..) => {
                    ModuleItem::Stmt(Stmt::Empty(EmptyStmt { span: DUMMY_SP }))
                }
                ModuleDecl::Import(..) => ModuleItem::ModuleDecl(decl),

                _ => unimplemented!("Unexported: {:?}", decl),
            },

            _ => item,
        }
    }
}
