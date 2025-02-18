//! Implementation of USDT functionality on platforms without runtime linker support.

// Copyright 2021 Oxide Computer Company

use crate::record::{emit_probe_record, process_section};
use crate::{common, wrap_probes_in_modules, Probe, Provider};
use dof::{serialize_section, Section};
use proc_macro2::TokenStream;
use quote::quote;
use std::convert::TryFrom;

/// Compile a DTrace provider definition into Rust tokens that implement its probes.
pub fn compile_provider_source(
    source: &str,
    config: &crate::CompileProvidersConfig,
) -> Result<TokenStream, crate::Error> {
    let dfile = dtrace_parser::File::try_from(source)?;
    let providers = dfile
        .providers()
        .iter()
        .map(|provider| {
            let provider = Provider::from(provider);
            compile_provider(&provider, &config)
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
    compile_provider(provider, config)
}

fn compile_provider(provider: &Provider, config: &crate::CompileProvidersConfig) -> TokenStream {
    let probe_impls = provider
        .probes
        .iter()
        .map(|probe| compile_probe(provider, probe, config))
        .collect::<Vec<_>>();
    wrap_probes_in_modules(config, provider, quote! { #(#probe_impls)* })
}

fn compile_probe(
    provider: &Provider,
    probe: &Probe,
    config: &crate::CompileProvidersConfig,
) -> TokenStream {
    let (unpacked_args, in_regs) = common::construct_probe_args(&probe.types);
    let is_enabled_rec = emit_probe_record(&provider.name, &probe.name, None);
    let probe_rec = emit_probe_record(&provider.name, &probe.name, Some(&probe.types));
    let pre_macro_block = TokenStream::new();
    let impl_block = quote! {
        {
            let mut is_enabled: u64;
            unsafe {
                asm!(
                    "990:   clr rax",
                    #is_enabled_rec,
                    out("rax") is_enabled,
                    options(nomem, nostack, preserves_flags)
                );
            }

            if is_enabled != 0 {
                #unpacked_args
                unsafe {
                    asm!(
                        "990:   nop",
                        #probe_rec,
                        #in_regs
                        options(nomem, nostack, preserves_flags)
                    );
                }
            }
        }
    };
    common::build_probe_macro(
        config,
        provider,
        &probe.name,
        &probe.types,
        pre_macro_block,
        impl_block,
    )
}

fn extract_probe_records_from_section() -> Result<Option<Section>, crate::Error> {
    extern "C" {
        #[link_name = "__start_set_dtrace_probes"]
        static dtrace_probes_start: usize;
        #[link_name = "__stop_set_dtrace_probes"]
        static dtrace_probes_stop: usize;
    }

    // Without this the illumos linker may decide to omit the symbols above that
    // denote the start and stop addresses for this section. Note that the variable
    // must be mutable, otherwise this will generate a read-only section with the
    // name `set_dtrace_probes`. The section containing the actual probe records is
    // writable (to implement one-time registration), so an immutable variable here
    // leads to _two_ sections, one writable and one read-only. A mutable variable
    // here ensures this ends up in a mutable section, the same as the probe records.
    #[cfg(target_os = "illumos")]
    #[link_section = "set_dtrace_probes"]
    #[used]
    static mut FORCE_LOAD: [u64; 0] = [];

    let data = unsafe {
        let start = (&dtrace_probes_start as *const usize) as usize;
        let stop = (&dtrace_probes_stop as *const usize) as usize;
        std::slice::from_raw_parts(start as *const u8, stop - start)
    };
    process_section(data)
}

pub fn register_probes() -> Result<(), crate::Error> {
    if let Some(ref section) = extract_probe_records_from_section()? {
        let module_name = section
            .providers
            .values()
            .next()
            .and_then(|provider| {
                provider.probes.values().next().and_then(|probe| {
                    crate::record::addr_to_info(probe.address)
                        .1
                        .map(|path| path.rsplit('/').next().map(String::from).unwrap_or(path))
                        .or_else(|| Some(format!("?{:#x}", probe.address)))
                })
            })
            .unwrap_or_else(|| String::from("unknown-module"));
        let mut modname = [0; 64];
        for (i, byte) in module_name.bytes().take(modname.len() - 1).enumerate() {
            modname[i] = byte as i8;
        }
        ioctl_section(&serialize_section(&section), modname).map_err(crate::Error::from)
    } else {
        Ok(())
    }
}

fn ioctl_section(buf: &[u8], modname: [std::os::raw::c_char; 64]) -> Result<(), std::io::Error> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let helper = dof::dof_bindings::dof_helper {
        dofhp_mod: modname,
        dofhp_addr: buf.as_ptr() as u64,
        dofhp_dof: buf.as_ptr() as u64,
    };
    let data = &helper as *const _;
    let cmd: i32 = 0x64746803;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dtrace/helper")?;
    if unsafe { libc::ioctl(file.as_raw_fd(), cmd, data) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
