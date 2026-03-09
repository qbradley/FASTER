//! Campaign reporting — structured output for seed exploration results.

use std::time::Duration;

/// Aggregated result of a [`SeedCampaign`](crate::campaign::SeedCampaign) run.
#[derive(Debug)]
pub struct CampaignReport {
    /// Total number of (scenario, seed) pairs executed.
    pub total: usize,
    /// Number that passed all invariants.
    pub passed: usize,
    /// Number that failed at least one invariant.
    pub failed: usize,
    /// Wall-clock time for the entire campaign.
    pub elapsed: Duration,
    /// Details of each failure.
    pub failures: Vec<ScenarioFailure>,
}

impl CampaignReport {
    /// Returns `true` if every scenario/seed pair passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// Human-readable summary suitable for terminal output.
    pub fn display_human(&self) -> String {
        let mut s = format!(
            "Campaign: {}/{} passed ({} failed) in {:.1}s\n",
            self.passed,
            self.total,
            self.failed,
            self.elapsed.as_secs_f64(),
        );
        for f in &self.failures {
            s.push_str(&format!(
                "  FAIL: {} seed={} — {}\n",
                f.scenario_name, f.seed, f.error_message,
            ));
            s.push_str(&format!(
                "    Reproduce: {}\n",
                Self::reproduction_command(f)
            ));
        }
        s
    }

    /// Machine-readable JSON output.
    pub fn display_json(&self) -> String {
        let mut j = String::new();
        j.push_str("{\n");
        j.push_str(&format!("  \"total\": {},\n", self.total));
        j.push_str(&format!("  \"passed\": {},\n", self.passed));
        j.push_str(&format!("  \"failed\": {},\n", self.failed));
        j.push_str(&format!(
            "  \"elapsed_ms\": {},\n",
            self.elapsed.as_millis()
        ));
        j.push_str("  \"failures\": [");
        if self.failures.is_empty() {
            j.push(']');
        } else {
            j.push('\n');
            for (i, f) in self.failures.iter().enumerate() {
                j.push_str("    {\n");
                j.push_str(&format!(
                    "      \"scenario\": \"{}\",\n",
                    escape_json(&f.scenario_name)
                ));
                j.push_str(&format!("      \"seed\": {},\n", f.seed));
                j.push_str(&format!(
                    "      \"invariant\": \"{}\",\n",
                    escape_json(&f.invariant_violated),
                ));
                j.push_str(&format!(
                    "      \"error\": \"{}\"\n",
                    escape_json(&f.error_message),
                ));
                if i < self.failures.len() - 1 {
                    j.push_str("    },\n");
                } else {
                    j.push_str("    }\n");
                }
            }
            j.push_str("  ]");
        }
        j.push_str("\n}");
        j
    }

    /// Cargo command to reproduce a single failure.
    pub fn reproduction_command(failure: &ScenarioFailure) -> String {
        format!(
            "cargo test -p faster-dst --test campaign_tests -- {} --seed={}",
            failure.scenario_name, failure.seed,
        )
    }
}

/// Details of a single (scenario, seed) failure.
#[derive(Debug, Clone)]
pub struct ScenarioFailure {
    /// Name of the scenario template.
    pub scenario_name: String,
    /// Seed that triggered the failure.
    pub seed: u64,
    /// Short description of the violated invariant.
    pub invariant_violated: String,
    /// Full error message.
    pub error_message: String,
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_all_passed() {
        let r = CampaignReport {
            total: 10,
            passed: 10,
            failed: 0,
            elapsed: Duration::from_millis(500),
            failures: vec![],
        };
        assert!(r.all_passed());
        assert!(r.display_human().contains("10/10 passed"));
    }

    #[test]
    fn report_with_failures() {
        let f = ScenarioFailure {
            scenario_name: "test_scenario".to_string(),
            seed: 42,
            invariant_violated: "recovery".to_string(),
            error_message: "key 5: expected 50, got NotFound".to_string(),
        };
        let r = CampaignReport {
            total: 10,
            passed: 9,
            failed: 1,
            elapsed: Duration::from_secs(1),
            failures: vec![f],
        };
        assert!(!r.all_passed());
        let human = r.display_human();
        assert!(human.contains("9/10 passed"));
        assert!(human.contains("seed=42"));
    }

    #[test]
    fn reproduction_command_format() {
        let f = ScenarioFailure {
            scenario_name: "checkpoint_crash".to_string(),
            seed: 99,
            invariant_violated: String::new(),
            error_message: String::new(),
        };
        let cmd = CampaignReport::reproduction_command(&f);
        assert!(cmd.contains("faster-dst"));
        assert!(cmd.contains("campaign_tests"));
        assert!(cmd.contains("checkpoint_crash"));
        assert!(cmd.contains("--seed=99"));
    }

    #[test]
    fn json_is_well_formed() {
        let f = ScenarioFailure {
            scenario_name: "s1".to_string(),
            seed: 1,
            invariant_violated: "inv".to_string(),
            error_message: "err with \"quotes\"".to_string(),
        };
        let r = CampaignReport {
            total: 2,
            passed: 1,
            failed: 1,
            elapsed: Duration::from_millis(100),
            failures: vec![f],
        };
        let json = r.display_json();
        assert!(json.contains("\"total\": 2"));
        assert!(json.contains("\"failed\": 1"));
        assert!(json.contains("\"seed\": 1"));
        // Quotes should be escaped
        assert!(json.contains("\\\"quotes\\\""));
    }

    #[test]
    fn json_empty_failures() {
        let r = CampaignReport {
            total: 5,
            passed: 5,
            failed: 0,
            elapsed: Duration::from_millis(50),
            failures: vec![],
        };
        let json = r.display_json();
        assert!(json.contains("\"failures\": []"));
    }
}
