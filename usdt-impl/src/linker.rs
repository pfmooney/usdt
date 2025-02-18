//! USDT implementation on platforms with linker support (macOS).
//!
//! On systems with linker support for the compile-time construction of DTrace
//! USDT probes we can lean heavily on those mechanisms. Rather than interpreting
//! the provider file ourselves, we invoke the system's `dtrace -h` to generate a C
//! header file. That header file contains the linker directives that convey
//! information from the provider definition such as types and stability. We parse
//! that header file and generate code that effectively reproduces in Rust the
//! equivalent of what we would see in C.
//!
//! For example, the header file might contain code like this:
//! ```ignore
//! #define FOO_STABILITY "___dtrace_stability$foo$v1$1_1_0_1_1_0_1_1_0_1_1_0_1_1_0"
//! #define FOO_TYPEDEFS "___dtrace_typedefs$foo$v2"
//!
//! #if !defined(DTRACE_PROBES_DISABLED) || !DTRACE_PROBES_DISABLED
//!
//! #define	FOO_BAR() \
//! do { \
//! 	__asm__ volatile(".reference " FOO_TYPEDEFS); \
//! 	__dtrace_probe$foo$bar$v1(); \
//! 	__asm__ volatile(".reference " FOO_STABILITY); \
//! } while (0)
//! ```
//!
//! In rust, we'll want the probe site to look something like this:
//! ```ignore
//! #![feature(asm)]
//! extern "C" {
//!     #[link_name = "__dtrace_stability$foo$v1$1_1_0_1_1_0_1_1_0_1_1_0_1_1_0"]
//!     fn stability();
//!     #[link_name = "__dtrace_probe$foo$bar$v1"]
//!     fn probe();
//!     #[link_name = "__dtrace_typedefs$foo$v2"]
//!     fn typedefs();
//!
//! }
//! unsafe {
//!     asm!(".reference {}", sym typedefs);
//!     probe();
//!     asm!(".reference {}", sym stability);
//! }
//! ```
//! There are a few things to note above:
//! 1. We cannot simply generate code with the symbol name embedded in the asm!
//!    block e.g. `asm!(".reference __dtrace_typedefs$foo$v2")`. The asm! macro
//!    removes '$' characters yielding the incorrect symbol.
//! 2. The header file stability and typedefs contain three '_'s whereas the
//!    Rust code has just two. The `sym <symbol_name>` apparently prepends an
//!    extra underscore in this case.
//! 3. The probe needs to be a function type (because we call it), but the types
//!    of the `stability` and `typedefs` symbols could be anything--we just need
//!    a symbol name we can reference for the asm! macro that won't get garbled.

// Copyright 2021 Oxide Computer Company

use crate::{common, wrap_probes_in_modules, DataType, Provider};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::{
    collections::BTreeMap,
    convert::TryFrom,
    io::Write,
    process::{Command, Stdio},
};

/// Compile a DTrace provider definition into Rust tokens that implement its probes.
pub fn compile_provider_source(
    source: &str,
    config: &crate::CompileProvidersConfig,
) -> Result<TokenStream, crate::Error> {
    let dfile = dtrace_parser::File::try_from(source)?;
    let header = build_header_from_provider(&source)?;
    let provider_info = extract_providers(&header);
    let providers = dfile
        .providers()
        .into_iter()
        .map(|provider| {
            let provider = Provider::from(provider);
            compile_provider(&provider, &provider_info[&provider.name], config)
        })
        .collect::<Vec<_>>();
    Ok(quote! {
        #(#providers)*
    })
}

pub fn compile_provider_from_definition(
    provider: &Provider,
    config: &crate::CompileProvidersConfig,
) -> TokenStream {
    // Unwrap safety: The type signature confirms that `provider` is valid.
    let header = build_header_from_provider(&provider.to_d_source()).unwrap();
    let provider_info = extract_providers(&header);
    let provider_tokens = compile_provider(provider, &provider_info[&provider.name], config);
    quote! {
        #provider_tokens
    }
}

fn compile_provider(
    provider: &Provider,
    provider_info: &ProviderInfo,
    config: &crate::CompileProvidersConfig,
) -> TokenStream {
    let mut probe_impls = Vec::new();
    for probe in provider.probes.iter() {
        probe_impls.push(compile_probe(
            provider,
            &probe.name,
            config,
            &provider_info.is_enabled[&probe.name],
            &provider_info.probes[&probe.name],
            &probe.types,
        ));
    }
    let stability = &provider_info.stability;
    let typedefs = &provider_info.typedefs;
    let tokens = quote! {
        extern "C" {
            // These are dummy symbols, which we declare so that we can name them inside the
            // probe macro via a valid Rust path, e.g., `$crate::#mod_name::stability`.
            // The macOS linker will actually define these symbols, which are required to
            // generate valid DOF.
            #[allow(unused)]
            #[link_name = #stability]
            pub(crate) fn stability();
            #[allow(unused)]
            #[link_name = #typedefs]
            pub(crate) fn typedefs();
        }
        #(#probe_impls)*
    };
    wrap_probes_in_modules(config, provider, tokens)
}

fn compile_probe(
    provider: &Provider,
    probe_name: &str,
    config: &crate::CompileProvidersConfig,
    is_enabled: &str,
    probe: &str,
    types: &[DataType],
) -> TokenStream {
    let mod_name = config.provider_module(&provider.name);
    let is_enabled_fn = format_ident!("{}_{}_enabled", &provider.name, probe_name);
    let probe_fn = config.probe_ident(&provider.name, probe_name);
    let extern_probe_fn = format_ident!("__{}", probe_fn);
    let ffi_param_list = types.iter().map(|typ| {
        let ty = typ.to_rust_ffi_type();
        syn::parse2::<syn::FnArg>(quote! { _: #ty }).unwrap()
    });
    let (unpacked_args, in_regs) = common::construct_probe_args(types);

    // Create identifiers for the stability and typedef symbols, used by Apple's linker.
    // Note that the Rust symbols these refer to are defined in the caller of this function.
    let stability_fn = format_ident!("stability");
    let typedef_fn = format_ident!("typedefs");

    let pre_macro_block = quote! {
        extern "C" {
            #[allow(unused)]
            #[link_name = #is_enabled]
            pub(crate) fn #is_enabled_fn() -> i32;
            #[allow(unused)]
            #[link_name = #probe]
            pub(crate) fn #extern_probe_fn(#(#ffi_param_list),*);
        }
    };

    #[cfg(target_arch = "x86_64")]
    let call_instruction = quote! { "call {extern_probe_fn}" };
    #[cfg(target_arch = "aarch64")]
    let call_instruction = quote! { "bl {extern_probe_fn}" };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("USDT only supports x86_64 and AArch64 architectures");

    let mod_name = if mod_name.is_empty() {
        quote! {}
    } else {
        quote! { #mod_name:: }
    };
    let impl_block = quote! {
        unsafe {
            if $crate:: #mod_name #is_enabled_fn() != 0 {
                #unpacked_args
                asm!(
                    ".reference {typedefs}",
                    #call_instruction,
                    ".reference {stability}",
                    typedefs = sym $crate:: #mod_name #typedef_fn,
                    extern_probe_fn = sym $crate:: #mod_name #extern_probe_fn,
                    stability = sym $crate:: #mod_name #stability_fn,
                    #in_regs
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
    };

    common::build_probe_macro(
        config,
        provider,
        probe_name,
        types,
        pre_macro_block,
        impl_block,
    )
}

#[derive(Debug, Default, Clone)]
struct ProviderInfo {
    pub stability: String,
    pub typedefs: String,
    pub is_enabled: BTreeMap<String, String>,
    pub probes: BTreeMap<String, String>,
}

fn extract_providers(header: &str) -> BTreeMap<String, ProviderInfo> {
    let mut providers = BTreeMap::new();
    for line in header.lines() {
        if let Some((provider_name, stability)) = is_stability_line(&line) {
            let mut info = ProviderInfo::default();
            info.stability = stability.to_string();
            providers.insert(provider_name.to_string(), info);
        }
        if let Some((provider_name, typedefs)) = is_typedefs_line(&line) {
            providers.get_mut(provider_name).unwrap().typedefs = typedefs.to_string();
        }
        if let Some((provider_name, probe_name, enabled)) = is_enabled_line(&line) {
            providers
                .get_mut(provider_name)
                .unwrap()
                .is_enabled
                .insert(probe_name.to_string(), enabled.to_string());
        }
        if let Some((provider_name, probe_name, probe)) = is_probe_line(&line) {
            providers
                .get_mut(provider_name)
                .unwrap()
                .probes
                .insert(probe_name.to_string(), probe.to_string());
        }
    }
    providers
}

// Return the (provider_name, stability) from a line, if it looks like the appropriate #define'd
// line from the autogenerated header file.
fn is_stability_line(line: &str) -> Option<(&str, &str)> {
    contains_needle(line, "___dtrace_stability$")
}

// Return the (provider_name, typedefs) from a line, if it looks like the appropriate #define'd
// line from the autogenerated header file.
fn is_typedefs_line(line: &str) -> Option<(&str, &str)> {
    contains_needle(line, "___dtrace_typedefs$")
}

fn contains_needle<'a>(line: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    if let Some(index) = line.find(needle) {
        let rest = &line[index + needle.len()..];
        let provider_end = rest.find("$").unwrap();
        let provider_name = &rest[..provider_end];
        // NOTE: The extra offset to the start index works as follows. The symbol name really needs
        // to be `___dtrace_stability$...`. But that symbol name will have a "_" prefixed to it
        // during compilation, so we remove the leading one here, knowing it will be added back.
        let needle = &line[index + 1..line.len() - 1];
        Some((provider_name, needle))
    } else {
        None
    }
}

// Return the (provider, probe, enabled) from a line, if it looks like the appropriate extern
// function declaration from the autogenerated header file.
fn is_enabled_line(line: &str) -> Option<(&str, &str, &str)> {
    contains_needle2(line, "extern int __dtrace_isenabled$")
}

// Return the (provider, probe, probe) from a line, if it looks like the appropriate extern
// function declaration from the autogenerated header file.
fn is_probe_line(line: &str) -> Option<(&str, &str, &str)> {
    contains_needle2(line, "extern void __dtrace_probe$")
}

fn contains_needle2<'a>(line: &'a str, needle: &str) -> Option<(&'a str, &'a str, &'a str)> {
    if let Some(index) = line.find(needle) {
        let rest = &line[index + needle.len()..];
        let provider_end = rest.find("$").unwrap();
        let provider_name = &rest[..provider_end];

        let rest = &rest[provider_end + 1..];
        let probe_end = rest.find("$").unwrap();
        let probe_name = &rest[..probe_end];

        let end = line.rfind("(").unwrap();
        let start = line.find(line.split(" ").nth(2).unwrap()).unwrap();
        let needle = &line[start..end];
        Some((provider_name, probe_name, needle))
    } else {
        None
    }
}

fn build_header_from_provider(source: &str) -> Result<String, crate::Error> {
    let mut child = Command::new("dtrace")
        .arg("-h")
        .arg("-s")
        .arg("/dev/stdin")
        .arg("-o")
        .arg("/dev/stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or(crate::Error::DTraceError)?;
        stdin
            .write_all(source.as_bytes())
            .map_err(|_| crate::Error::DTraceError)?;
    }
    let output = child.wait_with_output()?;
    String::from_utf8(output.stdout).map_err(|_| crate::Error::DTraceError)
}

pub fn register_probes() -> Result<(), crate::Error> {
    // This function is a NOP, since we're using Apple's linker to create the DOF and call ioctl(2)
    // to send it to the driver.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Probe;

    #[test]
    fn test_is_stability_line() {
        let line = "this line is ok \"___dtrace_stability$foo$bar\"";
        let result = is_stability_line(line);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "foo");
        assert_eq!(result.unwrap().1, "__dtrace_stability$foo$bar");
        assert!(is_stability_line("bad").is_none());
    }

    #[test]
    fn test_is_typedefs_line() {
        let line = "this line is ok \"___dtrace_typedefs$foo$bar\"";
        let result = is_typedefs_line(line);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "foo");
        assert_eq!(result.unwrap().1, "__dtrace_typedefs$foo$bar");
        assert!(is_typedefs_line("bad").is_none());
    }

    #[test]
    fn test_is_enabled_line() {
        let line = "extern int __dtrace_isenabled$foo$bar$xxx(void);";
        let result = is_enabled_line(line);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "foo");
        assert_eq!(result.unwrap().1, "bar");
        assert_eq!(result.unwrap().2, "__dtrace_isenabled$foo$bar$xxx");
        assert!(is_enabled_line("bad").is_none());
    }

    #[test]
    fn test_is_probe_line() {
        let line = "extern void __dtrace_probe$foo$bar$xxx(whatever);";
        let result = is_probe_line(line);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "foo");
        assert_eq!(result.unwrap().1, "bar");
        assert_eq!(result.unwrap().2, "__dtrace_probe$foo$bar$xxx");
        assert!(is_enabled_line("bad").is_none());
    }

    #[test]
    fn test_compile_probe() {
        let provider_name = "foo";
        let probe_name = "bar";
        let extern_probe_name = "__bar";
        let is_enabled = "__dtrace_isenabled$foo$bar$xxx";
        let probe = "__dtrace_probe$foo$bar$xxx";
        let types = vec![];
        let provider = Provider {
            name: provider_name.to_string(),
            probes: vec![Probe {
                name: probe_name.to_string(),
                types: types.clone(),
            }],
            use_statements: vec![],
        };
        let tokens = compile_probe(
            &provider,
            probe_name,
            &crate::CompileProvidersConfig::default(),
            is_enabled,
            probe,
            &types,
        );

        let output = tokens.to_string();

        let needle = format!("link_name = \"{is_enabled}\"", is_enabled = is_enabled);
        assert!(output.find(&needle).is_some());

        let needle = format!("link_name = \"{probe}\"", probe = probe);
        assert!(output.find(&needle).is_some());

        let needle = format!(
            "pub (crate) fn {provider_name}_{probe_name}",
            provider_name = provider_name,
            probe_name = probe_name
        );
        assert!(output.find(&needle).is_some());

        let needles = &[
            "asm ! (\".reference {typedefs}\"",
            "call {extern_probe_fn}",
            "\".reference {stability}",
            &format!(
                "typedefs = sym $ crate :: {provider_name} :: typedefs",
                provider_name = provider_name
            ),
            &format!(
                "probe_fn = sym $ crate :: {provider_name} :: {extern_probe_name}",
                provider_name = provider_name,
                extern_probe_name = extern_probe_name
            ),
            &format!(
                "stability = sym $ crate :: {provider_name} :: stability",
                provider_name = provider_name
            ),
        ];
        for needle in needles.iter() {
            println!("{}", needle);
            assert!(output.find(needle).is_some());
        }
    }
}
