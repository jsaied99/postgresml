use quote::{format_ident, quote, ToTokens};
use syn::{visit::Visit, DeriveInput, ItemImpl, Type};

use crate::common::{AttributeArgs, GetImplMethod};
use crate::types::{OutputType, SupportedType};

pub fn generate_custom_into_js_result(parsed: DeriveInput) -> proc_macro::TokenStream {
    let name = parsed.ident;
    let fields_named = match parsed.data {
        syn::Data::Struct(s) => match s.fields {
            syn::Fields::Named(n) => n,
            _ => panic!("custom_into_js proc_macro structs should only have named fields"),
        },
        _ => panic!("custom_into_js proc_macro should only be used on structs"),
    };

    let sets: Vec<proc_macro2::TokenStream> = fields_named
        .named
        .into_pairs()
        .map(|p| {
            let v = p.into_value();
            let name = v.ident.to_token_stream().to_string();
            let name_ident = v.ident;
            quote! {
                let js_item = self.#name_ident.into_js_result(cx)?;
                js_object.set(cx, #name, js_item)?;
            }
        })
        .collect();

    let out = quote! {
        impl IntoJsResult for #name {
            type Output = neon::types::JsObject;
            fn into_js_result<'a, 'b, 'c: 'b, C: neon::context::Context<'c>>(self, cx: &mut C) -> neon::result::JsResult<'b, Self::Output> {
                let js_object = cx.empty_object();
                {
                    use neon::object::Object;
                    #(#sets)*
                }
                Ok(js_object)
            }
        }
    };
    proc_macro::TokenStream::from(out)
}

pub fn generate_javascript_derive(parsed: DeriveInput) -> proc_macro::TokenStream {
    let name_ident = format_ident!("{}Javascript", parsed.ident);
    let wrapped_type_ident = format_ident!("{}", parsed.ident);

    let expanded = quote! {
        pub struct #name_ident {
            wrapped: #wrapped_type_ident
        }

        impl From<#wrapped_type_ident> for #name_ident {
            fn from(w: #wrapped_type_ident) -> Self {
                Self {
                    wrapped: w,
                }
            }
        }
    };
    proc_macro::TokenStream::from(expanded)
}

pub fn generate_javascript_methods(
    parsed: ItemImpl,
    attribute_args: &AttributeArgs,
) -> proc_macro::TokenStream {
    let mut methods = Vec::new();
    let mut object_sets = Vec::new();

    let wrapped_type_ident = match *parsed.self_ty {
        Type::Path(p) => p.path.segments.first().unwrap().ident.clone(),
        _ => panic!("Error getting struct ident for impl block"),
    };
    let name_ident = format_ident!("{}Javascript", wrapped_type_ident);

    // Iterate over the items - see: https://docs.rs/syn/latest/syn/enum.ImplItem.html
    for item in parsed.items {
        // We only create methods for functions listed in the attribute args
        match &item {
            syn::ImplItem::Fn(f) => {
                let method_name = f.sig.ident.to_string();
                if !attribute_args.args.contains(&method_name) {
                    continue;
                }
            }
            _ => continue,
        }

        // Get ImplMethod details - see: https://docs.rs/syn/latest/syn/visit/index.html
        let mut method = GetImplMethod::default();
        method.visit_impl_item(&item);
        if !method.exists {
            continue;
        }
        let method_ident = method.method_ident.clone();
        let (method_arguments, wrapper_arguments) =
            get_method_wrapper_arguments_javascript(&method);
        let (output_type, convert_from) = match &method.output_type {
            OutputType::Result(v) | OutputType::Other(v) => {
                convert_output_type_convert_from_javascript(v, &method)
            }
            OutputType::Default => (None, None),
        };

        let method_name_string = method_ident.to_string();
        object_sets.push(quote! {
            let f: Handle<JsFunction> = JsFunction::new(cx, #name_ident::#method_ident)?;
            obj.set(cx, #method_name_string, f)?;
        });


        let signature = quote! {
            pub fn #method_ident<'a>(mut cx: FunctionContext<'a>) -> #output_type
        };
        let prep_arguments = if let Some(_r) = &method.receiver {
            quote! {
                use core::ops::Deref;
                let this = cx.this();
                let s: Handle<JsBox<std::cell::RefCell<#name_ident>>> = this.get(&mut cx, "s")?;
                let wrapped = (*s).deref().borrow();
                let wrapped = wrapped.wrapped.clone();
                #(#method_arguments)*
            }
        } else {
            quote! {
                #(#method_arguments)*
            }
        };

        let wrapped_call = if method_name_string == "new" {
            quote! {
                #wrapped_type_ident::new(#(#wrapper_arguments),*)
            }
        } else {
            quote! {
                wrapped.#method_ident(#(#wrapper_arguments),*)
            }
        };

        let middle = if method.is_async {
            quote! {
                let runtime = crate::get_or_set_runtime();
                let x = runtime.block_on(#wrapped_call);

            }
        } else {
            quote! {
                let x = #wrapped_call; 
            }
        };
        let middle = if let OutputType::Result(_) = method.output_type {
            quote! {
                #middle
                let x = x.expect("Error in rust method");
            }
        } else {
            middle
        };
        let middle = if let Some(convert) = convert_from {
            quote! {
                #middle
                let x = #convert::from(x);
            }
        } else {
            middle
        };
        let mq = if method.is_async {
            quote! {
                #signature {
                    #prep_arguments
                    let channel = cx.channel();
                    let (deferred, promise) = cx.promise();
                    deferred.try_settle_with(&channel, move |mut cx| {
                        #middle
                        x.into_js_result(&mut cx)
                    }).expect("Error sending js");  
                    Ok(promise)
                }
            }
        } else {
            quote! {
                #signature {
                    #prep_arguments
                    #middle
                    x.into_js_result(&mut cx)
                }
            }
        };
        methods.push(mq);
    }

    proc_macro::TokenStream::from(quote! {
        impl #name_ident {
            #(#methods)*
        }

        impl IntoJsResult for #name_ident {
            type Output = neon::types::JsObject;
            fn into_js_result<'a, 'b, 'c: 'b, C: Context<'c>>(self, cx: &mut C) -> neon::result::JsResult<'b, Self::Output> {
                let obj = cx.empty_object();
                let s = cx.boxed(std::cell::RefCell::new(self));
                obj.set(cx, "s", s)?;
                #(#object_sets)*
                Ok(obj)
            }
        }
        impl Finalize for #name_ident {}
    })

    // proc_macro::TokenStream::from(quote! {})
}

fn get_method_wrapper_arguments_javascript(
    method: &GetImplMethod,
) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
    let mut wrapper_arguments = Vec::new();
    let mut method_arguments = Vec::new();

    method
        .method_arguments
        .iter()
        .enumerate()
        .for_each(|(i, (_argument_name, argument_type))| {
            let argument_ident = format_ident!("arg{}", i);
            let (argument_type_tokens, wrapper_argument_tokens) = convert_method_wrapper_arguments(
                argument_ident.clone(),
                argument_type,
            ); 
            let argument_type_js = get_javascript_type(argument_type);
            let method_argument = match argument_type {
                SupportedType::Option(_o) => quote! {
                    let #argument_ident = cx.argument_opt(#i as i32);
                    let #argument_ident = <#argument_type_tokens>::from_option_js_type(&mut cx, #argument_ident)?;
                },
                _ => quote! {
                    let #argument_ident = cx.argument::<#argument_type_js>(#i as i32)?;
                    let #argument_ident = <#argument_type_tokens>::from_js_type(&mut cx, #argument_ident)?;
                }
            };
            method_arguments.push(method_argument);
            wrapper_arguments.push(wrapper_argument_tokens.into_token_stream());
        });

    (method_arguments, wrapper_arguments)
}

fn convert_method_wrapper_arguments(
    name_ident: syn::Ident,
    ty: &SupportedType,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    match ty {
        SupportedType::Reference(r) => {
            let (d, w) = convert_method_wrapper_arguments(name_ident, r);
            (d, quote! { & #w})
        }
        SupportedType::str => (syn::parse_str::<syn::Type>("String").unwrap().into_token_stream(), quote! { #name_ident}),
        _ => {
            let t = ty
                .to_type()
                .expect("Could not parse type in convert_method_wrapper_arguments in javascript.rs");
            (t.into_token_stream(), quote! {#name_ident})
        }
    }
}

fn get_javascript_type(ty: &SupportedType) -> syn::Type {
    match ty {
        SupportedType::Reference(r) => get_javascript_type(r),
        SupportedType::str | SupportedType::String => syn::parse_str("JsString").unwrap(),
        SupportedType::Vec(_v) => syn::parse_str("JsArray").unwrap(),
        SupportedType::S => syn::parse_str("JsObject").unwrap(),
        SupportedType::Tuple(_t) => syn::parse_str("JsObject").unwrap(),
        SupportedType::HashMap((_k, _v)) => syn::parse_str("JsObject").unwrap(),
        SupportedType::i64 | SupportedType::f64 => syn::parse_str("JsNumber").unwrap(),
        // Our own types
        SupportedType::Database | SupportedType::Collection | SupportedType::Splitter => {
            syn::parse_str("JsObject").unwrap()
        }
        // Add more types as required
        _ => syn::parse_str("JsValue").unwrap(),
    }
}

pub fn convert_output_type_convert_from_javascript(
    ty: &SupportedType,
    method: &GetImplMethod,
) -> (
    Option<proc_macro2::TokenStream>,
    Option<proc_macro2::TokenStream>,
) {
    let (output_type, convert_from) = match ty {
        SupportedType::S => (
            Some(quote! {JsResult<'a, JsObject>}),
            Some(format_ident!("Self").into_token_stream()),
        ),
        t @ SupportedType::Database | t @ SupportedType::Collection => (
            Some(quote! {PyResult<&'a PyAny>}),
            Some(format_ident!("{}Javascript", t.to_string()).into_token_stream()),
        ),
        t => {
            let ty = get_javascript_type(t);
            (Some(quote! {JsResult<'a, #ty>}), None)
        }
    };

    if method.is_async {
        (Some(quote! {JsResult<'a, JsPromise>}), convert_from)
    } else {
        (output_type, convert_from)
    }
}
