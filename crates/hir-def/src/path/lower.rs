//! Transforms syntax into `Path` objects, ideally with accounting for hygiene

use std::iter;

use crate::{lower::LowerCtx, path::NormalPath, type_ref::ConstRef};

use hir_expand::{
    mod_path::resolve_crate_root,
    name::{AsName, Name},
};
use intern::{Interned, sym};
use syntax::ast::{self, AstNode, HasGenericArgs, HasTypeBounds};
use thin_vec::ThinVec;

use crate::{
    path::{
        AssociatedTypeBinding, GenericArg, GenericArgs, GenericArgsParentheses, ModPath, Path,
        PathKind,
    },
    type_ref::{LifetimeRef, TypeBound, TypeRef},
};

#[cfg(test)]
thread_local! {
    /// This is used to test `hir_segment_to_ast_segment()`. It's a hack, but it makes testing much easier.
    pub(super) static SEGMENT_LOWERING_MAP: std::cell::RefCell<rustc_hash::FxHashMap<ast::PathSegment, usize>> = std::cell::RefCell::default();
}

/// Converts an `ast::Path` to `Path`. Works with use trees.
/// It correctly handles `$crate` based path from macro call.
// If you modify the logic of the lowering, make sure to check if `hir_segment_to_ast_segment()`
// also needs an update.
pub(super) fn lower_path(ctx: &mut LowerCtx<'_>, mut path: ast::Path) -> Option<Path> {
    let mut kind = PathKind::Plain;
    let mut type_anchor = None;
    let mut segments = Vec::new();
    let mut generic_args = Vec::new();
    #[cfg(test)]
    let mut ast_segments = Vec::new();
    #[cfg(test)]
    let mut ast_segments_offset = 0;
    #[allow(unused_mut)]
    let mut push_segment = |_segment: &ast::PathSegment, segments: &mut Vec<Name>, name| {
        #[cfg(test)]
        ast_segments.push(_segment.clone());
        segments.push(name);
    };
    loop {
        let segment = path.segment()?;

        if segment.coloncolon_token().is_some() {
            kind = PathKind::Abs;
        }

        match segment.kind()? {
            ast::PathSegmentKind::Name(name_ref) => {
                if name_ref.text() == "$crate" {
                    if path.qualifier().is_some() {
                        // FIXME: Report an error.
                        return None;
                    }
                    break kind = resolve_crate_root(
                        ctx.db.upcast(),
                        ctx.span_map().span_for_range(name_ref.syntax().text_range()).ctx,
                    )
                    .map(PathKind::DollarCrate)
                    .unwrap_or(PathKind::Crate);
                }
                let name = name_ref.as_name();
                let args = segment
                    .generic_arg_list()
                    .and_then(|it| lower_generic_args(ctx, it))
                    .or_else(|| {
                        lower_generic_args_from_fn_path(
                            ctx,
                            segment.parenthesized_arg_list(),
                            segment.ret_type(),
                        )
                    })
                    .or_else(|| {
                        segment.return_type_syntax().map(|_| GenericArgs::return_type_notation())
                    });
                if args.is_some() {
                    generic_args.resize(segments.len(), None);
                    generic_args.push(args);
                }
                push_segment(&segment, &mut segments, name);
            }
            ast::PathSegmentKind::SelfTypeKw => {
                push_segment(&segment, &mut segments, Name::new_symbol_root(sym::Self_.clone()));
            }
            ast::PathSegmentKind::Type { type_ref, trait_ref } => {
                assert!(path.qualifier().is_none()); // this can only occur at the first segment

                let self_type = TypeRef::from_ast(ctx, type_ref?);

                match trait_ref {
                    // <T>::foo
                    None => {
                        type_anchor = Some(self_type);
                        kind = PathKind::Plain;
                    }
                    // <T as Trait<A>>::Foo desugars to Trait<Self=T, A>::Foo
                    Some(trait_ref) => {
                        let path = Path::from_src(ctx, trait_ref.path()?)?;
                        let mod_path = path.mod_path()?;
                        let path_generic_args = path.generic_args();
                        let num_segments = mod_path.segments().len();
                        kind = mod_path.kind;

                        segments.extend(mod_path.segments().iter().cloned().rev());
                        #[cfg(test)]
                        {
                            ast_segments_offset = mod_path.segments().len();
                        }
                        if let Some(path_generic_args) = path_generic_args {
                            generic_args.resize(segments.len() - num_segments, None);
                            generic_args.extend(Vec::from(path_generic_args).into_iter().rev());
                        } else {
                            generic_args.resize(segments.len(), None);
                        }

                        let self_type = GenericArg::Type(self_type);

                        // Insert the type reference (T in the above example) as Self parameter for the trait
                        let last_segment = generic_args.get_mut(segments.len() - num_segments)?;
                        *last_segment = Some(match last_segment.take() {
                            Some(it) => GenericArgs {
                                args: iter::once(self_type)
                                    .chain(it.args.iter().cloned())
                                    .collect(),

                                has_self_type: true,
                                bindings: it.bindings.clone(),
                                parenthesized: it.parenthesized,
                            },
                            None => GenericArgs {
                                args: Box::new([self_type]),
                                has_self_type: true,
                                ..GenericArgs::empty()
                            },
                        });
                    }
                }
            }
            ast::PathSegmentKind::CrateKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                kind = PathKind::Crate;
                break;
            }
            ast::PathSegmentKind::SelfKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                // don't break out if `self` is the last segment of a path, this mean we got a
                // use tree like `foo::{self}` which we want to resolve as `foo`
                if !segments.is_empty() {
                    kind = PathKind::SELF;
                    break;
                }
            }
            ast::PathSegmentKind::SuperKw => {
                let nested_super_count = if let PathKind::Super(n) = kind { n } else { 0 };
                kind = PathKind::Super(nested_super_count + 1);
            }
        }
        path = match qualifier(&path) {
            Some(it) => it,
            None => break,
        };
    }
    segments.reverse();
    if !generic_args.is_empty() || type_anchor.is_some() {
        generic_args.resize(segments.len(), None);
        generic_args.reverse();
    }

    if segments.is_empty() && kind == PathKind::Plain && type_anchor.is_none() {
        // plain empty paths don't exist, this means we got a single `self` segment as our path
        kind = PathKind::SELF;
    }

    // handle local_inner_macros :
    // Basically, even in rustc it is quite hacky:
    // https://github.com/rust-lang/rust/blob/614f273e9388ddd7804d5cbc80b8865068a3744e/src/librustc_resolve/macros.rs#L456
    // We follow what it did anyway :)
    if segments.len() == 1 && kind == PathKind::Plain {
        if let Some(_macro_call) = path.syntax().parent().and_then(ast::MacroCall::cast) {
            let syn_ctxt = ctx.span_map().span_for_range(path.segment()?.syntax().text_range()).ctx;
            if let Some(macro_call_id) = syn_ctxt.outer_expn(ctx.db) {
                if ctx.db.lookup_intern_macro_call(macro_call_id).def.local_inner {
                    kind = match resolve_crate_root(ctx.db.upcast(), syn_ctxt) {
                        Some(crate_root) => PathKind::DollarCrate(crate_root),
                        None => PathKind::Crate,
                    }
                }
            }
        }
    }

    #[cfg(test)]
    {
        ast_segments.reverse();
        SEGMENT_LOWERING_MAP
            .with_borrow_mut(|map| map.extend(ast_segments.into_iter().zip(ast_segments_offset..)));
    }

    let mod_path = Interned::new(ModPath::from_segments(kind, segments));
    if type_anchor.is_none() && generic_args.is_empty() {
        return Some(Path::BarePath(mod_path));
    } else {
        return Some(Path::Normal(Box::new(NormalPath {
            generic_args: generic_args.into_boxed_slice(),
            type_anchor,
            mod_path,
        })));
    }

    fn qualifier(path: &ast::Path) -> Option<ast::Path> {
        if let Some(q) = path.qualifier() {
            return Some(q);
        }
        // FIXME: this bottom up traversal is not too precise.
        // Should we handle do a top-down analysis, recording results?
        let use_tree_list = path.syntax().ancestors().find_map(ast::UseTreeList::cast)?;
        let use_tree = use_tree_list.parent_use_tree();
        use_tree.path()
    }
}

/// This function finds the AST segment that corresponds to the HIR segment
/// with index `segment_idx` on the path that is lowered from `path`.
pub fn hir_segment_to_ast_segment(path: &ast::Path, segment_idx: u32) -> Option<ast::PathSegment> {
    // Too tightly coupled to `lower_path()`, but unfortunately we cannot decouple them,
    // as keeping source maps for all paths segments will have a severe impact on memory usage.

    let mut segments = path.segments();
    if let Some(ast::PathSegmentKind::Type { trait_ref: Some(trait_ref), .. }) =
        segments.clone().next().and_then(|it| it.kind())
    {
        segments.next();
        return find_segment(trait_ref.path()?.segments().chain(segments), segment_idx);
    }
    return find_segment(segments, segment_idx);

    fn find_segment(
        segments: impl Iterator<Item = ast::PathSegment>,
        segment_idx: u32,
    ) -> Option<ast::PathSegment> {
        segments
            .filter(|segment| match segment.kind() {
                Some(
                    ast::PathSegmentKind::CrateKw
                    | ast::PathSegmentKind::SelfKw
                    | ast::PathSegmentKind::SuperKw
                    | ast::PathSegmentKind::Type { .. },
                )
                | None => false,
                Some(ast::PathSegmentKind::Name(name)) => name.text() != "$crate",
                Some(ast::PathSegmentKind::SelfTypeKw) => true,
            })
            .nth(segment_idx as usize)
    }
}

pub(super) fn lower_generic_args(
    lower_ctx: &mut LowerCtx<'_>,
    node: ast::GenericArgList,
) -> Option<GenericArgs> {
    let mut args = Vec::new();
    let mut bindings = Vec::new();
    for generic_arg in node.generic_args() {
        match generic_arg {
            ast::GenericArg::TypeArg(type_arg) => {
                let type_ref = TypeRef::from_ast_opt(lower_ctx, type_arg.ty());
                lower_ctx.update_impl_traits_bounds_from_type_ref(type_ref);
                args.push(GenericArg::Type(type_ref));
            }
            ast::GenericArg::AssocTypeArg(assoc_type_arg) => {
                if assoc_type_arg.param_list().is_some() {
                    // We currently ignore associated return type bounds.
                    continue;
                }
                if let Some(name_ref) = assoc_type_arg.name_ref() {
                    // Nested impl traits like `impl Foo<Assoc = impl Bar>` are allowed
                    lower_ctx.with_outer_impl_trait_scope(false, |lower_ctx| {
                        let name = name_ref.as_name();
                        let args = assoc_type_arg
                            .generic_arg_list()
                            .and_then(|args| lower_generic_args(lower_ctx, args))
                            .or_else(|| {
                                assoc_type_arg
                                    .return_type_syntax()
                                    .map(|_| GenericArgs::return_type_notation())
                            });
                        let type_ref =
                            assoc_type_arg.ty().map(|it| TypeRef::from_ast(lower_ctx, it));
                        let type_ref = type_ref
                            .inspect(|&tr| lower_ctx.update_impl_traits_bounds_from_type_ref(tr));
                        let bounds = if let Some(l) = assoc_type_arg.type_bound_list() {
                            l.bounds().map(|it| TypeBound::from_ast(lower_ctx, it)).collect()
                        } else {
                            Box::default()
                        };
                        bindings.push(AssociatedTypeBinding { name, args, type_ref, bounds });
                    });
                }
            }
            ast::GenericArg::LifetimeArg(lifetime_arg) => {
                if let Some(lifetime) = lifetime_arg.lifetime() {
                    let lifetime_ref = LifetimeRef::new(&lifetime);
                    args.push(GenericArg::Lifetime(lifetime_ref))
                }
            }
            ast::GenericArg::ConstArg(arg) => {
                let arg = ConstRef::from_const_arg(lower_ctx, Some(arg));
                args.push(GenericArg::Const(arg))
            }
        }
    }

    if args.is_empty() && bindings.is_empty() {
        return None;
    }
    Some(GenericArgs {
        args: args.into_boxed_slice(),
        has_self_type: false,
        bindings: bindings.into_boxed_slice(),
        parenthesized: GenericArgsParentheses::No,
    })
}

/// Collect `GenericArgs` from the parts of a fn-like path, i.e. `Fn(X, Y)
/// -> Z` (which desugars to `Fn<(X, Y), Output=Z>`).
fn lower_generic_args_from_fn_path(
    ctx: &mut LowerCtx<'_>,
    args: Option<ast::ParenthesizedArgList>,
    ret_type: Option<ast::RetType>,
) -> Option<GenericArgs> {
    let params = args?;
    let mut param_types = Vec::new();
    for param in params.type_args() {
        let type_ref = TypeRef::from_ast_opt(ctx, param.ty());
        param_types.push(type_ref);
    }
    let args = Box::new([GenericArg::Type(
        ctx.alloc_type_ref_desugared(TypeRef::Tuple(ThinVec::from_iter(param_types))),
    )]);
    let bindings = if let Some(ret_type) = ret_type {
        let type_ref = TypeRef::from_ast_opt(ctx, ret_type.ty());
        Box::new([AssociatedTypeBinding {
            name: Name::new_symbol_root(sym::Output.clone()),
            args: None,
            type_ref: Some(type_ref),
            bounds: Box::default(),
        }])
    } else {
        // -> ()
        let type_ref = ctx.alloc_type_ref_desugared(TypeRef::unit());
        Box::new([AssociatedTypeBinding {
            name: Name::new_symbol_root(sym::Output.clone()),
            args: None,
            type_ref: Some(type_ref),
            bounds: Box::default(),
        }])
    };
    Some(GenericArgs {
        args,
        has_self_type: false,
        bindings,
        parenthesized: GenericArgsParentheses::ParenSugar,
    })
}
