# Dependency review

GitHub reported inherited dependency alerts when the fork was created. The lockfile updates bytes to 1.11.1 and rand 0.8 to 0.8.6, the patched compatible versions for GHSA-434x-w66g-qw3r and GHSA-cq8v-f236-94qc.

Other inherited alerts remain tracked by GitHub. The current reqwest 0.11 dependency uses rustls 0.21 and rustls-webpki 0.101; replacing those requires a client dependency migration. The high-severity malformed-CRL panic advisory GHSA-82j2-j2ch-gfr8 requires explicitly enabled CRL checking and attacker-influenced CRLs. This application uses reqwest's default verifier and does not load CRLs. This is a reachability assessment, not a claim that the dependency is patched.

The low-severity atty alert has no patched release and enters through clap, env_logger and the benchmark harness. Migrating those dependencies is deferred. No alert was dismissed or hidden. Dependabot remains enabled.
