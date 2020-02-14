mod colors;

use self::colors::*;
use crate::notify::notify;

use apt_cli_wrappers::AptUpgradeEvent;
use chrono::{TimeZone, Utc};
use clap::ArgMatches;
use num_traits::FromPrimitive;
use pop_upgrade::{
    client,
    daemon::*,
    misc,
    recovery::{RecoveryEvent, ReleaseFlags as RecoveryReleaseFlags},
    release::{
        eol::{EolDate, EolStatus},
        systemd::{self, LoaderEntry},
        RefreshOp, UpgradeEvent, UpgradeMethod,
    },
};
use std::{
    convert::TryFrom,
    fs,
    io::{self, BufRead, Write},
    path::Path,
};
use ubuntu_version::{Codename, Version as UbuntuVersion};
use yansi::Paint;

const FETCH_RESULT_STR: &str = "Package fetch status";
const FETCH_RESULT_SUCCESS: &str = "cargo has been loaded successfully";
const FETCH_RESULT_ERROR: &str = "package-fetching aborted";

const RECOVERY_RESULT_STR: &str = "Recovery upgrade status";
const RECOVERY_RESULT_SUCCESS: &str = "recovery partition refueled and ready to go";
const RECOVERY_RESULT_ERROR: &str = "recovery upgrade aborted";

const UPGRADE_RESULT_STR: &str = "Release upgrade status";
const UPGRADE_RESULT_SUCCESS: &str = "systems are go for launch: reboot now";
const UPGRADE_RESULT_ERROR: &str = "release upgrade aborted";

#[derive(AsRef, Deref, DerefMut)]
#[as_ref]
#[deref]
#[deref_mut]
pub struct Client(client::Client);

impl Client {
    pub fn new() -> Result<Self, client::Error> { client::Client::new().map(Client) }

    /// Executes the recovery subcommand of the client.
    pub fn recovery(&mut self, matches: &ArgMatches) -> anyhow::Result<()> {
        match matches.subcommand() {
            ("default-boot", _) => {
                root_required()?;
                systemd::set_default_boot_variant(LoaderEntry::Recovery)?;
            }
            ("upgrade", Some(matches)) => {
                match matches.subcommand() {
                    ("from-release", Some(matches)) => {
                        let version = matches.value_of("VERSION").unwrap_or("");
                        let arch = matches.value_of("ARCH").unwrap_or("");
                        let flags = if matches.is_present("next") {
                            RecoveryReleaseFlags::NEXT
                        } else {
                            RecoveryReleaseFlags::empty()
                        };

                        self.recovery_upgrade_release(version, arch, flags)?;
                    }
                    ("from-file", Some(matches)) => {
                        let path = matches.value_of("PATH").expect("missing reqired PATH argument");

                        let _ = self.recovery_upgrade_file(path)?;
                    }
                    _ => unreachable!(),
                }

                self.event_listen_recovery_upgrade()?;
            }
            ("check", _) => {
                let version = self.recovery_version()?;
                pintln!(
                    "version: " (version.version) "\n"
                    "build: " (version.build)
                );
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    pub fn release(&mut self, matches: &ArgMatches) -> anyhow::Result<()> {
        match matches.subcommand() {
            ("dismiss", _) => {
                let devel = pop_upgrade::development_releases_enabled();
                let (_, _, _, _, is_lts) = self.release_check(devel)?;
                if is_lts {
                    self.dismiss_notification(DismissEvent::ByUser)?;
                } else {
                    println!("Only LTS releases may dismiss notifications");
                }
            }
            ("check", _) => {
                let (current, next, urgent, build, is_lts) = self.release_check(false)?;

                if atty::is(atty::Stream::Stdout) {
                    pintln!(
                        "      Current Release: " (current) "\n"
                        "         Next Release: " (next) "\n"
                        "New Release Available: "
                        if (build < 0) {
                            "false\n"
                        } else  {
                            (build) "\n"
                        }
                        "  Urgent Recovery ISO: "
                        if (urgent == -1) {
                            "None"
                        } else {
                            (chrono::NaiveDateTime::from_timestamp(urgent, 0))
                        }
                    );
                } else if build >= 0 {
                    if is_lts && (self.dismissed(&next) || self.dismiss_by_timestamp(&next)?) {
                        return Ok(());
                    }

                    let (summary, body) = notification_message(&current, &next);

                    notify(&summary, &body, || {
                        let _ =
                            exec::Command::new("gnome-control-center").arg("info-overview").exec();
                    });
                }
            }
            // Update the current system, without performing a release upgrade
            ("update", Some(matches)) => {
                let updates =
                    self.fetch_updates(Vec::new(), matches.is_present("download-only"))?;

                let client::Fetched { updates_available, completed, total } = updates;

                if !updates_available || total == 0 {
                    println!("no updates available to fetch");
                } else {
                    println!("fetching updates: {} of {} updates fetched", completed, total);
                    self.event_listen_fetch_updates()?;
                }
            }
            // Perform an upgrade to the next release. Supports either systemd or recovery upgrades.
            ("upgrade", Some(matches)) => {
                let (method, matches) = match matches.subcommand() {
                    ("systemd", Some(matches)) => (UpgradeMethod::Offline, matches),
                    ("recovery", Some(matches)) => (UpgradeMethod::Recovery, matches),
                    _ => unreachable!(),
                };

                let forcing =
                    matches.is_present("force-next") || pop_upgrade::development_releases_enabled();
                let (current, next, _urgent, available, _is_lts) = self.release_check(forcing)?;

                // Only upgrade if an upgrade is possible, or if being forced to upgrade.
                if forcing || available >= 0 {
                    // Before doing a release upgrade with the recovery partition, ensure that
                    // the recovery partition has been updated in advance.
                    if let UpgradeMethod::Recovery = method {
                        self.recovery_upgrade_release("", "", RecoveryReleaseFlags::empty())?;
                        self.event_listen_recovery_upgrade()?;
                    }

                    // Ask to perform the release upgrade, and then listen for its signals.
                    self.release_upgrade(method, current.as_ref(), next.as_ref())?;
                    let mut recall = self.event_listen_release_upgrade()?;

                    // Repeat as necessary.
                    while recall {
                        println!(
                            "{}: {}",
                            color_primary("Event"),
                            color_secondary("attempting to perform upgrade again")
                        );
                        self.release_upgrade(method, current.as_ref(), next.as_ref())?;
                        recall = self.event_listen_release_upgrade()?;
                    }

                    // Finalize the release upgrade.
                    self.release_upgrade_finalize()?;
                } else {
                    println!("no release available to upgrade to");
                }
            }
            // Set the recovery partition as the next boot target, and configure it to
            // automatically switch to the refresh view.
            ("refresh", Some(matches)) => {
                let action = match matches.subcommand() {
                    ("enable", _) => RefreshOp::Enable,
                    ("disable", _) => RefreshOp::Disable,
                    _ => {
                        self.refresh_os(RefreshOp::Status)?;
                        return Ok(());
                    }
                };

                self.refresh_os(action)?;
                println!("reboot to boot into the recovery partition to begin the refresh install");
            }
            ("repair", Some(_)) => {
                self.release_repair()?;
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    pub fn status(&mut self, _matches: &ArgMatches) -> anyhow::Result<()> {
        let info = self.0.status()?;

        let (status, sub_status) = match DaemonStatus::from_u8(info.status) {
            Some(status) => {
                let x = <&'static str>::from(status);
                let y = match status {
                    DaemonStatus::ReleaseUpgrade => match UpgradeEvent::from_u8(info.sub_status) {
                        Some(sub) => <&'static str>::from(sub),
                        None => "unknown sub_status",
                    },
                    DaemonStatus::RecoveryUpgrade => {
                        match RecoveryEvent::from_u8(info.sub_status) {
                            Some(sub) => <&'static str>::from(sub),
                            None => "unknown sub_status",
                        }
                    }
                    _ => "",
                };

                (x, y)
            }
            None => ("unknown status", ""),
        };

        if sub_status.is_empty() {
            println!("{}", status);
        } else {
            println!("{}: {}", status, sub_status);
        }

        Ok(())
    }

    /// Check if this release has already been dismissed
    fn dismissed(&self, next: &str) -> bool {
        Path::new(DISMISSED).exists() && {
            fs::read_to_string(DISMISSED)
                .map(|dismissed| dismissed.as_str() == next)
                .unwrap_or(false)
        }
    }

    /// Check if the release has been dismissed by timestamp, or can be.
    fn dismiss_by_timestamp(&mut self, next: &str) -> Result<bool, client::Error> {
        if !Path::new(INSTALL_DATE).exists() && installed_after_release(next) {
            info!("dismissing notification for the latest release automatically");
            let _ = self.dismiss_notification(DismissEvent::ByTimestamp)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn release_check(
        &mut self,
        force_next: bool,
    ) -> Result<(Box<str>, Box<str>, i64, i16, bool), client::Error> {
        let info = self.0.release_check(force_next)?;

        Ok((info.current, info.next, info.urgent, info.build, info.is_lts))
    }

    fn event_listen_fetch_updates(&mut self) -> Result<(), client::Error> {
        self.event_listen(
            DaemonStatus::FetchingPackages,
            client::Client::fetch_updates_status,
            |new_status| {
                log_result(
                    new_status.status,
                    FETCH_RESULT_STR,
                    FETCH_RESULT_SUCCESS,
                    FETCH_RESULT_ERROR,
                    &new_status.why,
                )
            },
            |_client, signal| {
                match signal {
                    client::Signal::PackageFetchResult(status) => {
                        log_result(
                            status.status,
                            "Package fetch status",
                            "cargo has been loaded successfully",
                            "package-fetching aborted",
                            &status.why,
                        );

                        return Ok(client::Continue(false));
                    }
                    client::Signal::PackageFetched(status) => {
                        println!(
                            "{} ({}/{}) {}",
                            color_primary("Fetched"),
                            color_info(status.completed),
                            color_info(status.total),
                            color_secondary(status.package)
                        );
                    }
                    client::Signal::PackageFetching(package) => {
                        println!("{} {}", color_primary("Fetching"), color_secondary(package));
                    }
                    client::Signal::PackageUpgrade(event) => {
                        if let Ok(event) = AptUpgradeEvent::from_dbus_map(event.into_iter()) {
                            write_apt_event(event);
                        } else {
                            error!("failed to unpack the upgrade event");
                        }
                    }
                    _ => (),
                }

                Ok(client::Continue(true))
            },
        )
    }

    fn event_listen_recovery_upgrade(&mut self) -> Result<(), client::Error> {
        let mut reset = false;

        self.event_listen(
            DaemonStatus::RecoveryUpgrade,
            client::Client::recovery_upgrade_release_status,
            |new_status| {
                log_result(
                    new_status.status,
                    RECOVERY_RESULT_STR,
                    RECOVERY_RESULT_SUCCESS,
                    RECOVERY_RESULT_ERROR,
                    &new_status.why,
                )
            },
            move |_client, signal| {
                match signal {
                    client::Signal::RecoveryDownloadProgress(progress) => {
                        print!(
                            "\r{} {}/{} {}",
                            color_primary("Fetched"),
                            color_info(progress.progress / 1024),
                            color_info(progress.total / 1024),
                            color_primary("MiB")
                        );

                        let _ = io::stdout().flush();
                    }
                    client::Signal::RecoveryEvent(event) => {
                        if reset {
                            reset = false;
                            println!();
                        }

                        println!(
                            "{}: {}",
                            color_primary("Recovery event"),
                            <&'static str>::from(event)
                        );
                    }
                    client::Signal::RecoveryResult(status) => {
                        if reset {
                            reset = false;
                            println!();
                        }

                        log_result(
                            status.status,
                            RECOVERY_RESULT_STR,
                            RECOVERY_RESULT_SUCCESS,
                            RECOVERY_RESULT_ERROR,
                            &status.why,
                        );

                        return Ok(client::Continue(false));
                    }
                    _ => (),
                }

                Ok(client::Continue(true))
            },
        )
    }

    fn event_listen_release_upgrade(&mut self) -> Result<bool, client::Error> {
        let recall = &mut false;

        let result = self.event_listen(
            DaemonStatus::ReleaseUpgrade,
            client::Client::release_upgrade_status,
            |new_status| {
                log_result(
                    new_status.status,
                    UPGRADE_RESULT_STR,
                    UPGRADE_RESULT_SUCCESS,
                    UPGRADE_RESULT_ERROR,
                    &new_status.why,
                )
            },
            |client, signal| {
                match signal {
                    client::Signal::PackageFetchResult(status) => {
                        log_result(
                            status.status,
                            FETCH_RESULT_STR,
                            FETCH_RESULT_SUCCESS,
                            FETCH_RESULT_ERROR,
                            &status.why,
                        );
                    }
                    client::Signal::PackageFetched(package) => {
                        println!(
                            "{} ({}/{}): {}",
                            color_primary("Fetched"),
                            color_info(package.completed),
                            color_info(package.total),
                            color_secondary(&package.package)
                        );
                    }
                    client::Signal::PackageFetching(package) => {
                        println!("{} {}", color_primary("Fetching"), color_secondary(&package));
                    }
                    client::Signal::PackageUpgrade(event) => {
                        match AptUpgradeEvent::from_dbus_map(event.clone().into_iter()) {
                            Ok(event) => write_apt_event(event),
                            Err(()) => error!("failed to unpack the upgrade event: {:?}", event),
                        }
                    }
                    client::Signal::ReleaseResult(status) => {
                        if !*recall {
                            log_result(
                                status.status,
                                UPGRADE_RESULT_STR,
                                UPGRADE_RESULT_SUCCESS,
                                UPGRADE_RESULT_ERROR,
                                &status.why,
                            );
                        }

                        return Ok(client::Continue(false));
                    }
                    client::Signal::ReleaseEvent(event) => {
                        println!(
                            "{}: {}",
                            color_primary("Event"),
                            color_secondary(<&'static str>::from(event))
                        );
                    }
                    client::Signal::NoConnection => {
                        println!(
                            "{}",
                            color_error(
                                "Failed to connect to an apt repository. You may not be connected \
                                 to the Internet."
                            )
                        );

                        let prompt = format!("    {} y/N", color_primary("Try again?"));

                        if prompt_message(&prompt, false) {
                            *recall = true;
                        } else {
                            return Ok(client::Continue(false));
                        }
                    }
                    client::Signal::RepoCompatError(err) => {
                        let client::RepoCompatError { success, failure } = err;
                        println!("{}:", color_error("Incompatible repositories detected"));

                        for (url, why) in &failure {
                            println!(
                                "    {}: {}:\n        {}",
                                color_error("Error"),
                                color_tertiary(url),
                                color_error_desc(why),
                            );
                        }

                        for url in success {
                            println!("    {}: {}", color_primary("Success"), color_tertiary(url));
                        }

                        println!("{}", color_primary("Requesting user input:"));

                        let repos = failure.iter().map(|(url, _)| url).map(|url| {
                            let prompt = format!(
                                "    {}: ({})? y/N",
                                color_secondary("Keep repository"),
                                color_tertiary(url)
                            );

                            (url, prompt_message(&prompt, false))
                        });

                        client.repo_modify(repos)?;

                        *recall = true;
                    }
                    _ => (),
                }

                Ok(client::Continue(true))
            },
        );

        if !*recall {
            result?;
        }

        Ok(*recall)
    }
}

/// If the next release's timestamp is less than the install time.
fn installed_after_release(next: &str) -> bool {
    match pop_upgrade::install::time() {
        Ok(install_time) => match next.find('.') {
            Some(pos) => {
                let (major, mut minor) = next.split_at(pos);
                minor = &minor[1..];

                match (major.parse::<u8>(), minor.parse::<u8>()) {
                    (Ok(major), Ok(minor)) => {
                        match Codename::try_from(UbuntuVersion { major, minor, patch: 0 }) {
                            Ok(codename) => {
                                return codename.release_timestamp() < install_time as u64
                            }
                            Err(()) => error!("version {} is invalid", next),
                        }
                    }
                    _ => error!(
                        "major ({}) and minor({}) version failed to parse as u8",
                        major, minor
                    ),
                }
            }
            None => error!("version {} is invalid", next),
        },
        Err(why) => error!("failed to get install time: {}", why),
    }

    false
}

fn notification_message(current: &str, next: &str) -> (String, String) {
    match EolDate::fetch() {
        Ok(eol) => match eol.status() {
            EolStatus::Exceeded => {
                return (
                    fomat!("Support for Pop!_OS " (current) " has ended"),
                    fomat!(
                        "Security and application updates are no longer provided for Pop!_OS "
                        (current) ". Upgrade to Pop!_OS " (next) " to keep your computer secure."
                    ),
                );
            }
            EolStatus::Imminent => {
                let (y, m, d) = eol.ymd;
                return (
                    fomat!(
                        "Support for Pop!_OS " (current) " ends "
                        (Utc.ymd(y as i32, m, d).format("%B %-d, %Y"))
                    ),
                    fomat!(
                        "This computer will soon stop receiving updates"
                        ". Upgrade to Pop!_OS " (next) " to keep your computer secure."
                    ),
                );
            }
            EolStatus::Ok => (),
        },
        Err(why) => error!("failed to fetch EOL date: {}", why),
    }

    ("Upgrade Available".into(), fomat!("Pop!_OS " (next) " is available to download"))
}

fn write_apt_event(event: AptUpgradeEvent) {
    match event {
        AptUpgradeEvent::Processing { package } => {
            println!("{} for {}", color_primary("Processing triggers"), color_secondary(package));
        }
        AptUpgradeEvent::Progress { percent } => {
            println!("{}: {}%", color_primary("Progress"), color_info(percent));
        }
        AptUpgradeEvent::SettingUp { package } => {
            println!("{} {}", color_primary("Setting up"), color_secondary(package));
        }
        AptUpgradeEvent::Unpacking { package, version, over } => {
            println!(
                "{} {} ({}) over ({})",
                color_primary("Unpacking"),
                color_secondary(package),
                color_info(version),
                color_info(over)
            );
        }
        AptUpgradeEvent::WaitingOnLock => {
            println!(
                "{} {}",
                color_primary("Waiting"),
                color_secondary("on a process holding an apt/dpkg lock file")
            );
        }
    }
}

fn log_result(
    status: u8,
    event: &'static str,
    success: &'static str,
    error: &'static str,
    why: &str,
) {
    let inner: String;

    println!(
        "{}: {}",
        color_info(event),
        if status == 0 {
            color_primary(success)
        } else {
            inner = format!("{}: {}", color_error(error), color_error_desc(why));

            Paint::wrapping(inner.as_str())
        }
    );
}

// Write a prompt to the terminal, and wait for an answer.
fn prompt_message(message: &str, default: bool) -> bool {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    let answer = &mut String::with_capacity(16);

    enum Answer {
        Continue,
        Break(bool),
    }

    let mut display_prompt = move || -> io::Result<Answer> {
        answer.clear();

        stdout.write_all(message.as_bytes())?;
        stdout.flush()?;

        stdin.read_line(answer)?;

        if answer.is_empty() {
            return Ok(Answer::Break(default));
        } else if answer.starts_with('y') || answer.starts_with('Y') || answer == "true" {
            return Ok(Answer::Break(true));
        } else if answer.starts_with('n') || answer.starts_with('N') || answer == "false" {
            return Ok(Answer::Break(false));
        }

        stdout.write_all(b"The answer must be either `y` or `n`.\n")?;
        Ok(Answer::Continue)
    };

    loop {
        match display_prompt() {
            Ok(Answer::Continue) => continue,
            Ok(Answer::Break(answer)) => break answer,
            Err(_why) => break default,
        }
    }
}

pub fn root_required() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() == 0 } {
        Ok(())
    } else {
        Err(anyhow!("root is required for this operation"))
    }
}
