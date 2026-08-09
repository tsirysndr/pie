//! Terminal reporting.
//!
//! Interactive terminals get a live spinner per step showing the most recent
//! line of build output, with the full log kept aside and only shown if the step
//! fails. CI logs (and `--verbose`) get every line, prefixed and dimmed, because
//! a spinner in a log file is noise.

use console::{style, Style, Term};
use indicatif::{ProgressBar, ProgressStyle};
use std::time::{Duration, Instant};

/// The palette. 256-colour codes rather than truecolor: every terminal worth
/// supporting handles them, and they degrade to something sane over ssh.
pub mod brand {
    use console::Style;

    pub const PINK: u8 = 198; // electric pink — the brand mark
    pub const TEAL: u8 = 44; // teal — things that are working or worked
    pub const PURPLE: u8 = 141; // purple — step names
    pub const RED: u8 = 197; // hot red-pink — failures
    pub const GREY: u8 = 245; // dim — build output and labels

    fn styled(color: u8) -> Style {
        Style::new().color256(color)
    }

    pub fn title(text: &str) -> String {
        styled(PINK).bold().apply_to(text).to_string()
    }

    pub fn name(text: &str) -> String {
        styled(PURPLE).apply_to(text).to_string()
    }

    pub fn ok(text: &str) -> String {
        styled(TEAL).bold().apply_to(text).to_string()
    }

    pub fn error(text: &str) -> String {
        styled(RED).bold().apply_to(text).to_string()
    }

    pub fn dim(text: &str) -> String {
        styled(GREY).apply_to(text).to_string()
    }

    /// Right-aligns before colouring: ANSI escapes have no width, so padding a
    /// styled string with `{:>14}` silently misaligns the column.
    pub fn label(text: &str, width: usize) -> String {
        dim(&format!("{text:>width$}"))
    }
}

/// How many lines of a failed step to replay. Enough to see a compiler error
/// with context, short enough that the failure itself stays on screen.
const FAILURE_TAIL: usize = 60;

pub struct Reporter {
    verbose: bool,
    interactive: bool,
}

impl Reporter {
    pub fn new(verbose: bool) -> Self {
        let term = Term::stdout();
        // A spinner is only ever useful when someone is watching it.
        let interactive = term.is_term() && !verbose && std::env::var_os("CI").is_none();
        Self {
            verbose,
            interactive,
        }
    }

    pub fn heading(&self, left: &str, right: &str) {
        println!("\n  {} {}", brand::title(left), brand::dim(right));
    }

    pub fn info(&self, label: &str, value: &str) {
        println!("  {} {value}", brand::label(label, 10));
    }

    pub fn note(&self, message: &str) {
        println!("  {} {message}", style("note").yellow());
    }

    pub fn step(&self, name: &str) -> Step {
        let bar = if self.interactive {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("  {spinner} {wide_msg}")
                    .expect("static template")
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"]),
            );
            bar.enable_steady_tick(Duration::from_millis(80));
            bar.set_message(brand::name(name));
            Some(bar)
        } else {
            println!("  {} {}", spinner_style().apply_to("▸"), brand::name(name));
            None
        };

        Step {
            name: name.to_string(),
            started: Instant::now(),
            bar,
            interactive: self.interactive,
            tail: Vec::new(),
            width: Term::stdout().size().1 as usize,
        }
    }

    pub fn success(&self, message: &str) {
        println!("\n  {} {}\n", brand::ok("✔"), brand::title(message));
    }

    #[allow(dead_code)]
    pub fn failure(&self, message: &str) {
        println!("\n  {} {}\n", brand::error("✖"), brand::error(message));
    }

    #[allow(dead_code)]
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }
}

fn spinner_style() -> Style {
    Style::new().color256(brand::TEAL)
}

pub struct Step {
    name: String,
    started: Instant,
    bar: Option<ProgressBar>,
    interactive: bool,
    tail: Vec<String>,
    width: usize,
}

impl Step {
    /// Feeds one line of build output to the reporter.
    pub fn line(&mut self, line: &str) {
        if self.tail.len() == FAILURE_TAIL {
            self.tail.remove(0);
        }
        self.tail.push(line.to_string());

        match &self.bar {
            Some(bar) => {
                let budget = self.width.saturating_sub(self.name.len() + 12).max(16);
                bar.set_message(format!(
                    "{} {}",
                    brand::name(&self.name),
                    brand::dim(&truncate(line.trim_end(), budget))
                ));
            }
            None => println!("  {} {}", brand::dim("│"), brand::dim(line.trim_end())),
        }
    }

    /// Prints a line that stays on screen even in spinner mode.
    pub fn say(&self, message: &str) {
        let render = || println!("  {} {}", brand::dim("│"), brand::dim(message));
        match &self.bar {
            Some(bar) => bar.suspend(render),
            None => render(),
        }
    }

    pub fn finish(mut self) {
        let elapsed = format_duration(self.started.elapsed());
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
        println!(
            "  {} {} {}",
            brand::ok("✔"),
            brand::name(&self.name),
            brand::dim(&elapsed)
        );
    }

    /// Prints the tail of the captured output, since in spinner mode the user
    /// has seen nothing but a one-line summary up to this point.
    pub fn fail(mut self) {
        let elapsed = format_duration(self.started.elapsed());
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
        println!(
            "  {} {} {}",
            brand::error("✖"),
            brand::error(&self.name),
            brand::dim(&elapsed)
        );

        if self.interactive && !self.tail.is_empty() {
            println!(
                "  {}",
                brand::dim(&format!("last {} lines:", self.tail.len()))
            );
            for line in &self.tail {
                println!("  {} {}", brand::error("│"), brand::dim(line.trim_end()));
            }
        }
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

pub fn format_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_scale() {
        assert_eq!(format_duration(Duration::from_secs_f64(1.5)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m 05s");
    }

    #[test]
    fn bytes_scale() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(28 * 1024 * 1024), "28.0 MiB");
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("héllo wörld", 8), "héllo w…");
    }

    #[test]
    fn labels_pad_before_colouring() {
        // The escape sequence must wrap the padding, not sit inside it.
        let label = brand::label("recipe", 10);
        assert!(label.contains("    recipe"));
    }
}
