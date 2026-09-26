.. list-table:: Where the sandbox measurements were taken
   :header-rows: 1
   :widths: 40 60

   * - Item
     - Value
   * - Machine
     - Node 1 (the daemon's own machine), Linux kernel 6.8.0-139-generic
   * - bubblewrap
     - bubblewrap 0.9.0
   * - kernel.apparmor_restrict_unprivileged_userns
     - 1 (1: only profiles such as bwrap's may create user namespaces)
   * - Host and Fridica probes
     - measured at every build (last 2026-09-26)
   * - Claude probe
     - 2.1.283 (Claude Code), measured 2026-09-26 (rerun with --probe-claude)
