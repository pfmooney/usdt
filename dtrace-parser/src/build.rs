//! Expose USDT probe points from Rust programs.
// Copyright 2021 Oxide Computer Company

use std::path::Path;

#[cfg(any(
    target_os = "macos",
    target_os = "illumos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
use std::{env, fs, path::PathBuf, process::Command};

use crate::parser::File;
use crate::DTraceError;

/// Build the FFI glue required to call DTrace probe points from Rust.
///
/// This function should be called in a `build.rs` script, given the path to a provider definition
/// file. This will ensure that the static library and FFI glue required to expose the probes to
/// Rust will be done prior to building the target crate.
#[cfg(any(
    target_os = "macos",
    target_os = "illumos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub fn build_providers<P: AsRef<Path>>(source: P) -> Result<(), DTraceError> {
    let source = source.as_ref().canonicalize().map_err(|e| {
        DTraceError::BuildError(format!("Could not canonicalize provider file: {}", e))
    })?;

    // Parse the actual D provider file
    let dfile = File::from_file(&source)?;

    // Generate the related filenames for source and built artifacts.
    let source_filename = source.to_str().ok_or(DTraceError::BuildError(
        "Invalid provider source file".to_string(),
    ))?;
    let source_basename = source
        .file_stem()
        .unwrap()
        .to_str()
        .ok_or(DTraceError::BuildError(
            "Invalid provider source file".to_string(),
        ))?;
    let header_name = format!("{}.h", source_basename);
    let source_name = format!("{}-wrapper.c", source_basename);
    let d_object_name = format!("{}.o", source_basename);
    let c_object_name = format!("{}-wrapper.o", source_basename);
    let lib_name = source_basename;

    // Everything is done relative to OUT_DIR
    let out_dir = PathBuf::from(
        env::var("OUT_DIR")
            .map_err(|_| DTraceError::BuildError("OUT_DIR is not set".to_string()))?,
    );
    let make_path = |name| {
        out_dir
            .join(&name)
            .to_str()
            .ok_or_else(|| DTraceError::BuildError(format!("Invalid filename: {}", name)))
            .map(String::from)
    };
    let header_path = make_path(&header_name)?;
    let source_path = make_path(&source_name)?;
    let c_object_path = make_path(&c_object_name)?;
    let d_object_path = make_path(&d_object_name)?;

    generate_provider_header(&source_filename, &header_path)?;
    write_c_source_file(&source_path, &dfile, &header_name)?;

    // Compile the autogenerated C source
    cc::Build::new()
        .cargo_metadata(false)
        .file(&source_path)
        .include(&out_dir)
        .try_compile(&c_object_name)
        .map_err(|e| DTraceError::BuildError(format!("Failed to build C object: {}", e)))?;

    // Run `dtrace -G -s provider.d source.o`. This generates a provider.o object, which
    // contains all the DTrace machinery to register the probes with the kernel. It also
    // modifies source.o, replacing the call instructions for any defined probes with NOP
    // instructions. Note that this step is not required on macOS systems.
    #[cfg(not(target_os = "macos"))]
    Command::new("dtrace")
        .arg("-G")
        .arg("-s")
        .arg(source_filename)
        .arg(&c_object_path)
        .arg("-o")
        .arg(&d_object_path)
        .output()
        .map_err(|e| {
            DTraceError::BuildError(format!(
                "Failed to run DTrace against compiled source file: {}",
                e
            ))
        })?;

    // Generate a static library from all the above artifacts.
    if cfg!(target_os = "macos") {
        cc::Build::new().object(&c_object_path).compile(lib_name);
    } else {
        cc::Build::new()
            .object(&c_object_path)
            .object(&d_object_path)
            .compile(lib_name);
    }

    // Notify cargo when to rerun the D provider file changes. The library is automatically
    // linked in by the cc::Build step.
    println!("cargo:rerun-if-changed={}", source_filename);
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "illumos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
pub fn build_providers<P: AsRef<Path>>(_source: P) -> Result<(), DTraceError> {
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum ExpandFormat {
    /// Expand probes to the Rust side of the FFI glue code.
    Rust,

    /// Expand probes to their corresponding C declarations.
    Declaration,

    /// Expand probes to the C side of the FFI glue code.
    Definition,
}

/// Expand the probe functions into the autogenerated FFI components.
///
/// This function returns the formatted code that comprises the Rust and C sides of the FFI used to
/// fire DTrace probes from a Rust program.
pub fn expand<P: AsRef<Path>>(source: P, format: ExpandFormat) -> Result<String, DTraceError> {
    let file = File::from_file(source.as_ref())?;
    Ok(format!(
        "{}",
        match format {
            ExpandFormat::Rust => file.to_rust_impl(),
            ExpandFormat::Declaration => file.to_c_declaration(),
            ExpandFormat::Definition => file.to_c_definition(),
        }
    ))
}

// Build and write out C FFI implementation file.
#[cfg(any(
    target_os = "macos",
    target_os = "illumos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn write_c_source_file(
    source_path: &String,
    dfile: &File,
    header_name: &str,
) -> Result<(), DTraceError> {
    let c_source = &[
        format!(
            "// Autogenerated C wrappers to DTrace probes in \"{}\"\n",
            dfile.name()
        ),
        String::from("#include <stdint.h>"),
        String::from("#include <stdlib.h>"),
        String::from("#include <string.h>"),
        String::from("#include <stdio.h>"),
        String::from("#include <assert.h>"),
        format!("#include \"{}\"\n", header_name),
        dfile.to_c_definition(),
    ]
    .join("\n");

    fs::write(&source_path, c_source)
        .map_err(|_| DTraceError::BuildError("Could not write C wrapper source file".into()))
}

#[cfg(any(
    target_os = "macos",
    target_os = "illumos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn generate_provider_header(source_filename: &str, header_path: &str) -> Result<(), DTraceError> {
    Command::new("dtrace")
        .arg("-h")
        .arg("-s")
        .arg(source_filename)
        .arg("-o")
        .arg(header_path)
        .output()
        .map_err(|_| DTraceError::BuildError("Failed to generate header from provider file".into()))?;
    Ok(())
}
