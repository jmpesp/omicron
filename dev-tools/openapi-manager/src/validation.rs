// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::apis::ApiBoundary;
use crate::apis::ManagedApi;
use crate::environment::Environment;
use crate::spec_files_generated::GeneratedApiSpecFile;
use anyhow::Context;
use atomicwrites::AtomicFile;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use openapi_manager_types::ValidationBackend;
use openapi_manager_types::ValidationContext;
use openapiv3::OpenAPI;
use std::io::Write;

pub fn validate(
    env: &Environment,
    api: &ManagedApi,
    generated: &GeneratedApiSpecFile,
) -> anyhow::Result<Vec<(Utf8PathBuf, CheckStatus)>> {
    let openapi = generated.openapi();
    let validation_result = validate_generated_openapi_document(api, &openapi)?;
    let extra_files = validation_result
        .extra_files
        .into_iter()
        .map(|(path, contents)| {
            let full_path = env.workspace_root.join(&path);
            let status = check_file(full_path, contents)?;
            Ok((path, status))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(extra_files)
}

fn validate_generated_openapi_document(
    api: &ManagedApi,
    openapi_doc: &OpenAPI,
) -> anyhow::Result<ValidationResult> {
    // Check for lint errors.
    let errors = match api.boundary() {
        ApiBoundary::Internal => openapi_lint::validate(&openapi_doc),
        ApiBoundary::External => openapi_lint::validate_external(&openapi_doc),
    };
    if !errors.is_empty() {
        return Err(anyhow::anyhow!("{}", errors.join("\n\n")));
    }

    // Perform any additional API-specific validation.
    let mut validation_context =
        ValidationContextImpl { errors: Vec::new(), files: Vec::new() };
    api.extra_validation(
        &openapi_doc,
        ValidationContext::new(&mut validation_context),
    );

    if !validation_context.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "OpenAPI document extended validation failed:\n{}",
            validation_context
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    Ok(ValidationResult { extra_files: validation_context.files })
}

/// Check a file against expected contents.
fn check_file(
    full_path: Utf8PathBuf,
    contents: Vec<u8>,
) -> anyhow::Result<CheckStatus> {
    let existing_contents =
        read_opt(&full_path).context("failed to read contents on disk")?;

    match existing_contents {
        Some(existing_contents) if existing_contents == contents => {
            Ok(CheckStatus::Fresh)
        }
        Some(existing_contents) => {
            Ok(CheckStatus::Stale(CheckStale::Modified {
                full_path,
                actual: existing_contents,
                expected: contents,
            }))
        }
        None => Ok(CheckStatus::Stale(CheckStale::New { expected: contents })),
    }
}

pub fn read_opt(path: &Utf8Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs_err::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => return Err(err),
    }
}

#[derive(Debug)]
#[must_use]
pub(crate) enum OverwriteStatus {
    Updated,
    Unchanged,
}

/// Overwrite a file with new contents, if the contents are different.
///
/// The file is left unchanged if the contents are the same. That's to avoid
/// mtime-based recompilations.
pub fn overwrite_file(
    path: &Utf8Path,
    contents: &[u8],
) -> anyhow::Result<OverwriteStatus> {
    // Only overwrite the file if the contents are actually different.
    let existing_contents =
        read_opt(path).context("failed to read contents on disk")?;

    // None means the file doesn't exist, in which case we always want to write
    // the new contents.
    if existing_contents.as_deref() == Some(contents) {
        return Ok(OverwriteStatus::Unchanged);
    }

    // Make sure the parent directory exists before trying to write any files.
    // N.B. that it's very unlikely that `parent()` would be `None` --- why are
    // you putting your OpenAPI document in `/`? --- but we may as well not fail
    // if that is the case...
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs_err::create_dir_all(path).with_context(|| {
                format!("failed to create parent directory for '{}'", path)
            })?
        }
    }

    AtomicFile::new(path, atomicwrites::OverwriteBehavior::AllowOverwrite)
        .write(|f| f.write_all(contents))
        .with_context(|| format!("failed to write to `{}`", path))?;

    Ok(OverwriteStatus::Updated)
}
#[derive(Debug)]
#[must_use]
pub(crate) enum CheckStatus {
    Fresh,
    Stale(CheckStale),
}

#[derive(Debug)]
#[must_use]
pub(crate) enum CheckStale {
    Modified { full_path: Utf8PathBuf, actual: Vec<u8>, expected: Vec<u8> },
    New { expected: Vec<u8> },
}

#[derive(Debug)]
#[must_use]
pub struct ValidationResult {
    // Extra files recorded by the validation context.
    extra_files: Vec<(Utf8PathBuf, Vec<u8>)>,
}

struct ValidationContextImpl {
    errors: Vec<anyhow::Error>,
    files: Vec<(Utf8PathBuf, Vec<u8>)>,
}

impl ValidationBackend for ValidationContextImpl {
    fn report_error(&mut self, error: anyhow::Error) {
        self.errors.push(error);
    }

    fn record_file_contents(&mut self, path: Utf8PathBuf, contents: Vec<u8>) {
        self.files.push((path, contents));
    }
}
