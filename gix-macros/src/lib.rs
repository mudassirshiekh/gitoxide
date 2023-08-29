use std::collections::HashMap;

use proc_macro::TokenStream;
use quote::quote;
use syn::{punctuated::Punctuated, spanned::Spanned, *};

#[derive(Copy, Clone)]
// All conversions we support.  Check references to this type for an idea how to add more.
enum Conversion<'a> {
    Into(&'a Type),
    AsRef(&'a Type),
    AsMut(&'a Type),
}

impl<'a> Conversion<'a> {
    fn conversion_expr(&self, i: &Ident) -> Expr {
        match *self {
            Conversion::Into(_) => parse_quote!(#i.into()),
            Conversion::AsRef(_) => parse_quote!(#i.as_ref()),
            Conversion::AsMut(_) => parse_quote!(#i.as_mut()),
        }
    }
}

fn parse_bounded_type(ty: &Type) -> Option<Ident> {
    match &ty {
        Type::Path(TypePath { qself: None, path }) if path.segments.len() == 1 => Some(path.segments[0].ident.clone()),
        _ => None,
    }
}

fn parse_bounds(bounds: &Punctuated<TypeParamBound, Token![+]>) -> Option<Conversion> {
    if bounds.len() != 1 {
        return None;
    }
    if let TypeParamBound::Trait(ref tb) = bounds.first().unwrap() {
        if let Some(seg) = tb.path.segments.iter().last() {
            if let PathArguments::AngleBracketed(ref gen_args) = seg.arguments {
                if let GenericArgument::Type(ref arg_ty) = gen_args.args.first().unwrap() {
                    if seg.ident == "Into" {
                        return Some(Conversion::Into(arg_ty));
                    } else if seg.ident == "AsRef" {
                        return Some(Conversion::AsRef(arg_ty));
                    } else if seg.ident == "AsMut" {
                        return Some(Conversion::AsMut(arg_ty));
                    }
                }
            }
        }
    }
    None
}

// create a map from generic type to Conversion
fn parse_generics(decl: &Signature) -> HashMap<Ident, Conversion<'_>> {
    let mut ty_conversions = HashMap::new();
    for gp in decl.generics.params.iter() {
        if let GenericParam::Type(ref tp) = gp {
            if let Some(conversion) = parse_bounds(&tp.bounds) {
                ty_conversions.insert(tp.ident.clone(), conversion);
            }
        }
    }
    if let Some(ref wc) = decl.generics.where_clause {
        for wp in wc.predicates.iter() {
            if let WherePredicate::Type(ref pt) = wp {
                if let Some(ident) = parse_bounded_type(&pt.bounded_ty) {
                    if let Some(conversion) = parse_bounds(&pt.bounds) {
                        ty_conversions.insert(ident, conversion);
                    }
                }
            }
        }
    }
    ty_conversions
}

fn convert<'a>(
    inputs: &'a Punctuated<FnArg, Token![,]>,
    ty_conversions: &HashMap<Ident, Conversion<'a>>,
) -> (Punctuated<FnArg, Token![,]>, Punctuated<Expr, Token![,]>, bool) {
    let mut argtypes = Punctuated::new();
    let mut argexprs = Punctuated::new();
    let mut has_self = false;
    inputs.iter().enumerate().for_each(|(i, input)| match input.clone() {
        FnArg::Receiver(receiver) => {
            has_self = true;
            argtypes.push(FnArg::Receiver(receiver));
        }
        FnArg::Typed(mut pat_type) => {
            let pat_ident = match &mut *pat_type.pat {
                Pat::Ident(pat_ident) if pat_ident.by_ref.is_none() && pat_ident.subpat.is_none() => pat_ident,
                _ => {
                    pat_type.pat = Box::new(Pat::Ident(PatIdent {
                        ident: Ident::new(&format!("arg_{i}_gen_by_momo_"), proc_macro2::Span::call_site()),
                        attrs: Default::default(),
                        by_ref: None,
                        mutability: None,
                        subpat: None,
                    }));

                    if let Pat::Ident(pat_ident) = &mut *pat_type.pat {
                        pat_ident
                    } else {
                        panic!()
                    }
                }
            };
            // Outer function type argument pat does not need mut unless its
            // type is `impl AsMut`.
            pat_ident.mutability = None;

            let ident = &pat_ident.ident;

            let to_expr = || parse_quote!(#ident);

            match *pat_type.ty {
                Type::ImplTrait(TypeImplTrait { ref bounds, .. }) => {
                    if let Some(conv) = parse_bounds(bounds) {
                        argexprs.push(conv.conversion_expr(ident));
                        if let Conversion::AsMut(_) = conv {
                            pat_ident.mutability = Some(Default::default());
                        }
                    } else {
                        argexprs.push(to_expr());
                    }
                }
                Type::Path(..) => {
                    if let Some(conv) = parse_bounded_type(&pat_type.ty).and_then(|ident| ty_conversions.get(&ident)) {
                        argexprs.push(conv.conversion_expr(ident));
                        if let Conversion::AsMut(_) = conv {
                            pat_ident.mutability = Some(Default::default());
                        }
                    } else {
                        argexprs.push(to_expr());
                    }
                }
                _ => {
                    argexprs.push(to_expr());
                }
            }

            // Now that mutability is decided, push the type into argtypes
            argtypes.push(FnArg::Typed(pat_type));
        }
    });
    (argtypes, argexprs, has_self)
}

fn contains_self_type_path(path: &Path) -> bool {
    path.segments.iter().any(|segment| {
        segment.ident == "Self"
            || match &segment.arguments {
                PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) => {
                    args.iter().any(|generic_arg| match generic_arg {
                        GenericArgument::Type(ty) => contains_self_type(ty),
                        GenericArgument::Const(expr) => contains_self_type_expr(expr),
                        _ => false,
                    })
                }
                PathArguments::Parenthesized(ParenthesizedGenericArguments { inputs, output, .. }) => {
                    inputs.iter().any(contains_self_type)
                        || matches!(output, ReturnType::Type(_, ty) if contains_self_type(ty))
                }
                _ => false,
            }
    })
}

fn contains_self_type_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Path(ExprPath { qself: Some(_), .. }) => true,
        Expr::Path(ExprPath { path, .. }) => contains_self_type_path(path),
        _ => false,
    }
}

fn contains_self_type(input: &Type) -> bool {
    match input {
        Type::Array(TypeArray { elem, len, .. }) => {
            // Call `matches!` first so that we can do tail call here
            // as an optimization.
            contains_self_type_expr(len) || contains_self_type(elem)
        }
        Type::Group(TypeGroup { elem, .. }) => contains_self_type(elem),
        Type::Paren(TypeParen { elem, .. }) => contains_self_type(elem),
        Type::Ptr(TypePtr { elem, .. }) => contains_self_type(elem),
        Type::Reference(TypeReference { elem, .. }) => contains_self_type(elem),
        Type::Slice(TypeSlice { elem, .. }) => contains_self_type(elem),

        Type::Tuple(TypeTuple { elems, .. }) => elems.iter().any(contains_self_type),

        Type::Path(TypePath { qself: Some(_), .. }) => true,
        Type::Path(TypePath { path, .. }) => contains_self_type_path(path),

        _ => false,
    }
}

fn has_self_type(input: &FnArg) -> bool {
    match input {
        FnArg::Receiver(_) => true,
        FnArg::Typed(PatType { ty, .. }) => contains_self_type(ty),
    }
}

/// Generate lightweight monomorphized wrapper around main implementation.
/// May be applied to functions and methods.
#[proc_macro_attribute]
pub fn momo(_attrs: TokenStream, input: TokenStream) -> TokenStream {
    //TODO: alternatively parse ImplItem::Method
    momo_inner(input.into()).into()
}

fn momo_inner(code: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let fn_item: Item = match syn::parse2(code.clone()) {
        Ok(input) => input,
        Err(err) => return err.to_compile_error(),
    };

    if let Item::Fn(item_fn) = fn_item {
        let ty_conversions = parse_generics(&item_fn.sig);
        let (argtypes, argexprs, has_self) = convert(&item_fn.sig.inputs, &ty_conversions);

        let uses_self = has_self
            || item_fn.sig.inputs.iter().any(has_self_type)
            || matches!(&item_fn.sig.output, ReturnType::Type(_, ty) if contains_self_type(ty));

        let inner_ident = Ident::new(
            // Use long qualifier to avoid name colision.
            &format!("_{}_inner_generated_by_gix_macro_momo", item_fn.sig.ident),
            proc_macro2::Span::call_site(),
        );

        let outer_sig = Signature {
            inputs: argtypes,
            ..item_fn.sig.clone()
        };

        let new_inner_item = Item::Fn(ItemFn {
            // Remove doc comment since they will increase compile-time and
            // also generates duplicate warning/error messages for the doc,
            // especially if it contains doc-tests.
            attrs: {
                let mut attrs = item_fn.attrs.clone();
                attrs.retain(|attr| {
                    let segments = &attr.path().segments;
                    !(segments.len() == 1 && segments[0].ident == "doc")
                });
                attrs
            },
            vis: Visibility::Inherited,
            sig: Signature {
                ident: inner_ident.clone(),
                ..item_fn.sig
            },
            block: item_fn.block,
        });

        if uses_self {
            // We can use `self` or `Self` inside function defined within
            // the impl-fn, so instead declare two separate functions.
            //
            // Since it's an impl block, it's unlikely to have name conflict,
            // though this won't work for impl-trait.
            //
            // This approach also make sure we can call the right function
            // using `Self` qualifier.
            let new_item = Item::Fn(ItemFn {
                attrs: item_fn.attrs,
                vis: item_fn.vis,
                sig: outer_sig,
                block: if has_self {
                    parse_quote!({ self.#inner_ident(#argexprs) })
                } else {
                    parse_quote!({ Self::#inner_ident(#argexprs) })
                },
            });
            quote!(#new_item #[allow(unused_mut)] #new_inner_item)
        } else {
            // Put the new inner function within the function block
            // to avoid duplicate function name and support associated
            // function that doesn't use `self` or `Self`.
            let new_item = Item::Fn(ItemFn {
                attrs: item_fn.attrs,
                vis: item_fn.vis,
                sig: outer_sig,
                block: parse_quote!({
                    #[allow(unused_mut)]
                    #new_inner_item

                    #inner_ident(#argexprs)
                }),
            });
            quote!(#new_item)
        }
    } else {
        Error::new(fn_item.span(), "expect a function").to_compile_error()
    }
}
