//! Cross-platform `WebDriver` server for Tauri applications.
//!
//! This is a [`WebDriver` Intermediary Node](https://www.w3.org/TR/webdriver/#dfn-intermediary-nodes) that works with [tauri-plugin-webdriver](https://github.com/Choochmeque/tauri-plugin-webdriver) to provide `WebDriver` automation for [Tauri](https://github.com/tauri-apps/tauri) apps. Your `WebDriver` client connects to `tauri-webdriver`, which launches your Tauri app and proxies requests to the embedded plugin. It requires two separate ports since two distinct [`WebDriver` Remote Ends](https://www.w3.org/TR/webdriver/#dfn-remote-ends) run.

#![doc(
    html_logo_url = "https://github.com/tauri-apps/tauri/raw/dev/.github/icon.png",
    html_favicon_url = "https://github.com/tauri-apps/tauri/raw/dev/.github/icon.png"
)]

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod cli;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod server;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() {
    println!("tauri-webdriver is not supported on this platform");
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn main() {
    let args: cli::Args = pico_args::Arguments::from_env().into();

    #[cfg(windows)]
    let _job_handle = {
        let job = match win32job::Job::create() {
            Ok(job) => job,
            Err(e) => {
                eprintln!("failed to create job object: {e}");
                std::process::exit(1);
            }
        };
        let mut info = match job.query_extended_limit_info() {
            Ok(info) => info,
            Err(e) => {
                eprintln!("failed to query job info: {e}");
                std::process::exit(1);
            }
        };
        info.limit_kill_on_job_close();
        if let Err(e) = job.set_extended_limit_info(&info) {
            eprintln!("failed to set job info: {e}");
            std::process::exit(1);
        }
        if let Err(e) = job.assign_current_process() {
            eprintln!("failed to assign process to job: {e}");
            std::process::exit(1);
        }
        job
    };

    // All platforms use plugin mode - the plugin runs inside the Tauri app
    if let Err(e) = server::run_plugin_mode(args) {
        eprintln!("error while running server: {e}");
        std::process::exit(1);
    }
}
