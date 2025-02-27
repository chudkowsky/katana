use proc_macro::TokenStream;
use quote::quote;
use syn::{LitStr, parse_macro_input};

/// Converts a string literal to a contract address.
///
/// # Example
/// ```
/// let addr = address!("0x123");
/// ```
#[proc_macro]
pub fn address(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as LitStr);
    let str_value = input.value();
    let path = crate_path();

    quote! {
        unsafe { #path::ContractAddress::from_raw_unchecked(#path::felt!(#str_value))  }
    }
    .into()
}

fn crate_path() -> &'static str {
    "::katana_primitives"
}
