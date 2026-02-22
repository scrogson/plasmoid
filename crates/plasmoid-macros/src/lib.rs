use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn, ItemStruct};

/// Attribute macro for particle entry points.
///
/// On a function: generates `mod bindings`, `Guest` impl, and `export!`.
/// On a struct: generates `mod bindings` and `use` import only.
#[proc_macro_attribute]
pub fn main(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Try parsing as a function first
    let item_clone = item.clone();
    if let Ok(func) = syn::parse::<ItemFn>(item_clone) {
        return expand_main_fn(func);
    }

    // Try parsing as a struct
    let item_clone = item.clone();
    if let Ok(item_struct) = syn::parse::<ItemStruct>(item_clone) {
        return expand_main_struct(item_struct);
    }

    syn::Error::new_spanned(
        proc_macro2::TokenStream::from(item),
        "#[plasmoid_sdk::main] can only be applied to a function or a struct",
    )
    .to_compile_error()
    .into()
}

fn is_string_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(last) = type_path.path.segments.last() {
            return last.ident == "String";
        }
    }
    false
}

fn expand_main_fn(func: ItemFn) -> TokenStream {
    let body = &func.block;
    let has_args = !func.sig.inputs.is_empty();

    let guest_fn = if !has_args {
        quote! {
            fn start() -> Result<(), String> #body
        }
    } else {
        // Extract param name and type
        let param = func.sig.inputs.first().unwrap();
        match param {
            syn::FnArg::Typed(pat_type) => {
                let param_name = &pat_type.pat;
                let param_type = &pat_type.ty;

                if is_string_type(param_type) {
                    // String param — pass through directly
                    quote! {
                        fn start(#param_name: String) -> Result<(), String> #body
                    }
                } else {
                    // Typed param — auto-deserialize from JSON
                    quote! {
                        fn start(__plasmoid_init_args: String) -> Result<(), String> {
                            let #param_name: #param_type =
                                plasmoid_sdk::from_init_args(&__plasmoid_init_args)?;
                            #body
                        }
                    }
                }
            }
            _ => {
                quote! {
                    fn start() -> Result<(), String> #body
                }
            }
        }
    };

    let output = quote! {
        #[allow(warnings)]
        mod bindings;
        use crate::bindings::plasmoid::runtime::process::*;

        struct __PlasmoidComponent;

        impl crate::bindings::Guest for __PlasmoidComponent {
            #guest_fn
        }

        crate::bindings::export!(__PlasmoidComponent with_types_in crate::bindings);
    };

    output.into()
}

fn expand_main_struct(item_struct: ItemStruct) -> TokenStream {
    let output = quote! {
        #[allow(warnings)]
        mod bindings;
        use crate::bindings::plasmoid::runtime::process::*;

        #item_struct
    };

    output.into()
}

/// Attribute macro for GenServer-style actors.
///
/// Applied to an `impl` block. Generates:
/// - `mod bindings` and `use` imports
/// - The receive loop as a `Guest` impl
/// - Static client methods (`call`, `cast`)
/// - `export!` macro invocation
#[proc_macro_attribute]
pub fn gen_server(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let impl_block = parse_macro_input!(item as syn::ItemImpl);
    expand_gen_server(impl_block)
        .unwrap_or_else(|e| e.to_compile_error().into())
}

fn expand_gen_server(impl_block: syn::ItemImpl) -> Result<TokenStream, syn::Error> {
    let struct_name = match impl_block.self_ty.as_ref() {
        syn::Type::Path(tp) => tp.path.segments.last().unwrap().ident.clone(),
        _ => {
            return Err(syn::Error::new_spanned(
                &impl_block.self_ty,
                "expected a struct name",
            ))
        }
    };

    // Detect which handlers are present and extract their types
    let mut has_init = false;
    let mut has_handle_call = false;
    let mut has_handle_cast = false;
    let mut has_handle_info = false;
    let mut call_req_ty: Option<syn::Type> = None;
    let mut call_reply_ty: Option<syn::Type> = None;
    let mut cast_msg_ty: Option<syn::Type> = None;

    for item in &impl_block.items {
        if let syn::ImplItem::Fn(method) = item {
            let name = method.sig.ident.to_string();
            match name.as_str() {
                "init" => {
                    has_init = true;
                }
                "handle_call" => {
                    has_handle_call = true;
                    // Extract request type from second param (after &mut self)
                    if let Some(arg) = method.sig.inputs.iter().nth(1) {
                        if let syn::FnArg::Typed(pat_type) = arg {
                            call_req_ty = Some((*pat_type.ty).clone());
                        }
                    }
                    // Extract reply type from return type
                    if let syn::ReturnType::Type(_, ty) = &method.sig.output {
                        call_reply_ty = Some((**ty).clone());
                    }
                }
                "handle_cast" => {
                    has_handle_cast = true;
                    if let Some(arg) = method.sig.inputs.iter().nth(1) {
                        if let syn::FnArg::Typed(pat_type) = arg {
                            cast_msg_ty = Some((*pat_type.ty).clone());
                        }
                    }
                }
                "handle_info" => {
                    has_handle_info = true;
                }
                _ => {}
            }
        }
    }

    // Generate state initialization
    let init_state = if has_init {
        quote! {
            let mut state = #struct_name::init(init_args)?;
        }
    } else {
        quote! {
            let mut state = <#struct_name as Default>::default();
        }
    };

    // Generate start function signature
    let start_sig = if has_init {
        quote! { fn start(init_args: String) -> Result<(), String> }
    } else {
        quote! { fn start() -> Result<(), String> }
    };

    // Generate tagged message handler (handle_call)
    let tagged_handler = if has_handle_call {
        quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Tagged(tagged)) => {
                let payload = &tagged.payload;
                if payload.len() < 4 { continue; }
                let pid_len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                if payload.len() < 4 + pid_len { continue; }
                let pid_str = match core::str::from_utf8(&payload[4..4 + pid_len]) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let request_bytes = &payload[4 + pid_len..];
                let request = match plasmoid_sdk::messaging::decode(request_bytes) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let response = state.handle_call(request);
                let response_bytes = plasmoid_sdk::messaging::encode(&response);
                if let Some(from) = crate::bindings::plasmoid::runtime::process::resolve(pid_str) {
                    let _ = crate::bindings::plasmoid::runtime::process::send_ref(
                        &from,
                        tagged.ref_,
                        &response_bytes,
                    );
                }
            }
        }
    } else {
        quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Tagged(_)) => {}
        }
    };

    // Generate data message handler (handle_cast + handle_info)
    let data_handler = match (has_handle_cast, has_handle_info) {
        (true, true) => quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Data(data)) => {
                if !data.is_empty() && data[0] == 0x01u8 {
                    let msg = match plasmoid_sdk::messaging::decode(&data[1..]) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match state.handle_cast(msg) {
                        plasmoid_sdk::CastResult::Stop => return Ok(()),
                        plasmoid_sdk::CastResult::Continue => {}
                    }
                } else {
                    match state.handle_info(data) {
                        plasmoid_sdk::CastResult::Stop => return Ok(()),
                        plasmoid_sdk::CastResult::Continue => {}
                    }
                }
            }
        },
        (true, false) => quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Data(data)) => {
                if !data.is_empty() && data[0] == 0x01u8 {
                    let msg = match plasmoid_sdk::messaging::decode(&data[1..]) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match state.handle_cast(msg) {
                        plasmoid_sdk::CastResult::Stop => return Ok(()),
                        plasmoid_sdk::CastResult::Continue => {}
                    }
                }
            }
        },
        (false, true) => quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Data(data)) => {
                match state.handle_info(data) {
                    plasmoid_sdk::CastResult::Stop => return Ok(()),
                    plasmoid_sdk::CastResult::Continue => {}
                }
            }
        },
        (false, false) => quote! {
            Some(crate::bindings::plasmoid::runtime::process::Message::Data(_)) => {}
        },
    };

    // Generate the receive loop
    let recv_loop = quote! {
        struct __PlasmoidComponent;

        impl crate::bindings::Guest for __PlasmoidComponent {
            #start_sig {
                #init_state

                loop {
                    match crate::bindings::plasmoid::runtime::process::recv(None) {
                        #tagged_handler
                        #data_handler
                        Some(_) => {}
                        None => return Ok(()),
                    }
                }
            }
        }

        crate::bindings::export!(__PlasmoidComponent with_types_in crate::bindings);
    };

    // Generate client methods
    let call_method = if has_handle_call {
        let req_ty = call_req_ty.as_ref().unwrap();
        let reply_ty = call_reply_ty.as_ref().unwrap();
        quote! {
            pub fn call(
                target: &crate::bindings::plasmoid::runtime::process::Pid,
                req: &#req_ty,
                timeout_ms: Option<u64>,
            ) -> Result<#reply_ty, plasmoid_sdk::CallError> {
                let ref_id = crate::bindings::plasmoid::runtime::process::make_ref();
                let self_pid_str = crate::bindings::plasmoid::runtime::process::self_pid().to_string();
                let req_bytes = plasmoid_sdk::messaging::encode(req);
                let mut payload = Vec::new();
                payload.extend((self_pid_str.len() as u32).to_le_bytes());
                payload.extend(self_pid_str.as_bytes());
                payload.extend(req_bytes);
                crate::bindings::plasmoid::runtime::process::send_ref(target, ref_id, &payload)
                    .map_err(|_| plasmoid_sdk::CallError::SendFailed)?;
                match crate::bindings::plasmoid::runtime::process::recv_ref(ref_id, timeout_ms) {
                    Some(crate::bindings::plasmoid::runtime::process::Message::Tagged(tagged)) => {
                        plasmoid_sdk::messaging::decode(&tagged.payload)
                            .map_err(|e| plasmoid_sdk::CallError::Decode(e))
                    }
                    _ => Err(plasmoid_sdk::CallError::Timeout),
                }
            }
        }
    } else {
        quote! {}
    };

    let cast_method = if has_handle_cast {
        let cast_ty = cast_msg_ty.as_ref().unwrap();
        quote! {
            pub fn cast(
                target: &crate::bindings::plasmoid::runtime::process::Pid,
                msg: &#cast_ty,
            ) -> Result<(), crate::bindings::plasmoid::runtime::process::SendError> {
                let mut payload = vec![0x01u8];
                payload.extend(plasmoid_sdk::messaging::encode(msg));
                crate::bindings::plasmoid::runtime::process::send(target, &payload)
            }
        }
    } else {
        quote! {}
    };

    let client_impl = if has_handle_call || has_handle_cast {
        quote! {
            impl #struct_name {
                #call_method
                #cast_method
            }
        }
    } else {
        quote! {}
    };

    let output = quote! {
        #[allow(warnings)]
        mod bindings;
        use crate::bindings::plasmoid::runtime::process::*;

        #impl_block

        #client_impl

        #recv_loop
    };

    Ok(output.into())
}
