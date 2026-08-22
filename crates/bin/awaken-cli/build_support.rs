#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConsoleCommand {
    pub(crate) program: &'static str,
    pub(crate) args: &'static [&'static str],
}

pub(crate) fn console_commands(windows: bool) -> [ConsoleCommand; 2] {
    let program = if windows { "pnpm.cmd" } else { "pnpm" };
    [
        ConsoleCommand {
            program,
            args: &["install", "--frozen-lockfile"],
        },
        ConsoleCommand {
            program,
            args: &["build"],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_build_repairs_partial_dependency_state_before_building() {
        // Cause/effect decision table: node_modules absent, complete, or left partial by an
        // interrupted install all select the same frozen install before build; the host OS only
        // selects the executable name. This makes every retry converge instead of treating a
        // directory created before failure as proof that dependency installation committed.
        for (windows, program) in [(false, "pnpm"), (true, "pnpm.cmd")] {
            assert_eq!(
                console_commands(windows),
                [
                    ConsoleCommand {
                        program,
                        args: &["install", "--frozen-lockfile"],
                    },
                    ConsoleCommand {
                        program,
                        args: &["build"],
                    },
                ]
            );
        }
    }
}
