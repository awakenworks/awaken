//! Native entry point for the portable host-resource ACP fixture. The existing
//! runtime executes an argv, not a shell shim, on every supported OS.
fn main() {
    let role = std::env::current_exe().unwrap();
    let status = std::process::Command::new(env!("AWAKEN_FIXTURE_NODE"))
        .arg(env!("AWAKEN_FIXTURE_SCRIPT"))
        .arg(role.file_stem().unwrap())
        .args(std::env::args_os().skip(1))
        .status()
        .expect("start fixture Node process");
    std::process::exit(status.code().unwrap_or(1));
}
