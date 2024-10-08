// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{io::Write, process::ExitCode};

use anyhow::Result;
use indent_write::io::IndentWriter;
use owo_colors::OwoColorize;
use similar::TextDiff;

use crate::{
    output::{
        display_api_spec, display_api_spec_file, display_error,
        display_summary, headers::*, plural, write_diff, OutputOpts, Styles,
    },
    spec::{all_apis, CheckStale, Environment},
    FAILURE_EXIT_CODE, NEEDS_UPDATE_EXIT_CODE,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum CheckResult {
    Success,
    NeedsUpdate,
    Failures,
}

impl CheckResult {
    pub(crate) fn to_exit_code(self) -> ExitCode {
        match self {
            CheckResult::Success => ExitCode::SUCCESS,
            CheckResult::NeedsUpdate => NEEDS_UPDATE_EXIT_CODE.into(),
            CheckResult::Failures => FAILURE_EXIT_CODE.into(),
        }
    }
}

pub(crate) fn check_impl(
    env: &Environment,
    output: &OutputOpts,
) -> Result<CheckResult> {
    let mut styles = Styles::default();
    if output.use_color(supports_color::Stream::Stderr) {
        styles.colorize();
    }

    let all_apis = all_apis();
    let total = all_apis.len();
    let count_width = total.to_string().len();
    let count_section_indent = count_section_indent(count_width);
    let continued_indent = continued_indent(count_width);

    eprintln!("{:>HEADER_WIDTH$}", SEPARATOR);

    eprintln!(
        "{:>HEADER_WIDTH$} {} OpenAPI {}...",
        CHECKING.style(styles.success_header),
        total.style(styles.bold),
        plural::documents(total),
    );
    let mut num_fresh = 0;
    let mut num_stale = 0;
    let mut num_failed = 0;

    for (ix, spec) in all_apis.iter().enumerate() {
        let count = ix + 1;

        match spec.check(env) {
            Ok(status) => {
                let total_errors = status.total_errors();
                let total_errors_width = total_errors.to_string().len();

                if total_errors == 0 {
                    // Success case.
                    let extra = if status.extra_files_len() > 0 {
                        format!(
                            ", {} extra files",
                            status.extra_files_len().style(styles.bold)
                        )
                    } else {
                        "".to_string()
                    };

                    eprintln!(
                        "{:>HEADER_WIDTH$} [{count:>count_width$}/{total}] {}: {}{extra}",
                        FRESH.style(styles.success_header),
                        display_api_spec(spec, &styles),
                        display_summary(&status.summary, &styles),
                    );

                    num_fresh += 1;
                    continue;
                }

                // Out of date: print errors.
                eprintln!(
                    "{:>HEADER_WIDTH$} [{count:>count_width$}/{total}] {}",
                    STALE.style(styles.warning_header),
                    display_api_spec(spec, &styles),
                );
                num_stale += 1;

                for (error_ix, (spec_file, error)) in
                    status.iter_errors().enumerate()
                {
                    let error_count = error_ix + 1;

                    let display_heading = |heading: &str| {
                        eprintln!(
                            "{:>HEADER_WIDTH$}{count_section_indent}\
                             ({error_count:>total_errors_width$}/{total_errors}) {}",
                             heading.style(styles.warning_header),
                            display_api_spec_file(spec, spec_file, &styles),
                        );
                    };

                    match error {
                        CheckStale::Modified {
                            full_path,
                            actual,
                            expected,
                        } => {
                            display_heading(MODIFIED);

                            let diff =
                                TextDiff::from_lines(&**actual, &**expected);
                            write_diff(
                                &diff,
                                &full_path,
                                &styles,
                                // Add an indent to align diff with the status message.
                                &mut IndentWriter::new(
                                    &continued_indent,
                                    std::io::stderr(),
                                ),
                            )?;
                        }
                        CheckStale::New => {
                            display_heading(NEW);
                        }
                    }
                }
            }
            Err(error) => {
                eprint!(
                    "{:>HEADER_WIDTH$} [{count:>count_width$}/{total}] {}",
                    FAILURE.style(styles.failure_header),
                    display_api_spec(spec, &styles),
                );
                let display = display_error(&error, styles.failure);
                write!(
                    IndentWriter::new(&continued_indent, std::io::stderr()),
                    "{}",
                    display,
                )?;

                num_failed += 1;
            }
        };
    }

    eprintln!("{:>HEADER_WIDTH$}", SEPARATOR);

    let status_header = if num_failed > 0 {
        FAILURE.style(styles.failure_header)
    } else if num_stale > 0 {
        STALE.style(styles.warning_header)
    } else {
        SUCCESS.style(styles.success_header)
    };

    eprintln!(
        "{:>HEADER_WIDTH$} {} {} checked: {} fresh, {} stale, {} failed",
        status_header,
        total.style(styles.bold),
        plural::documents(total),
        num_fresh.style(styles.bold),
        num_stale.style(styles.bold),
        num_failed.style(styles.bold),
    );
    if num_failed > 0 {
        eprintln!(
            "{:>HEADER_WIDTH$} (fix failures, then run {} to update)",
            "",
            "cargo xtask openapi generate".style(styles.bold)
        );
        Ok(CheckResult::Failures)
    } else if num_stale > 0 {
        eprintln!(
            "{:>HEADER_WIDTH$} (run {} to update)",
            "",
            "cargo xtask openapi generate".style(styles.bold)
        );
        Ok(CheckResult::NeedsUpdate)
    } else {
        Ok(CheckResult::Success)
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use crate::spec::Environment;

    use super::*;

    #[test]
    fn check_apis_up_to_date() -> Result<ExitCode> {
        let output = OutputOpts { color: clap::ColorChoice::Auto };
        let dir = Environment::new(None)?;

        let result = check_impl(&dir, &output)?;
        Ok(result.to_exit_code())
    }
}
