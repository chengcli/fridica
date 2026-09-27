Security model, updated
=======================

The 0.3 model stands: tokens never reach agents, the parent has no tools, workers run in their
backend's sandbox or in bubblewrap, a writable workspace may not contain Fridica or its config,
links are followed only into configured channels, artifacts are read only from inside a workspace
with symlinks resolved, and every model input is data. 0.4 adds three principals and one language
change, and each is bounded as follows.

.. list-table:: Principals and their reach
   :header-rows: 1
   :widths: 18 30 52

   * - Principal
     - Secrets it holds
     - Reach
   * - the daemon
     - Slack tokens, the F1 read-only GitHub token, the MCP key (to verify)
     - Slack, the state database, machines through transports
   * - a worker
     - none (``secret_env`` strips Slack, GitHub and MCP secrets; ``FRIDICA_WORKER=1`` is set)
     - its workspace under its policy; the scoped repository fetch for its item's remotes
   * - the overseer
     - its own fine-grained GitHub token (owner's repos: contents write, pull requests and actions
       write; upstream: contents read, pull requests and issues write); per-repository deploy keys
       for other people's forks; never Slack tokens
     - GitHub as above; the daemon's control API as ``actor = overseer`` (no settings routes); git on
       machines only inside jobs whose wrapper limits push refspecs
   * - the MCP server
     - the MCP key (to present)
     - the control API as ``actor = owner-desktop``; control tools only when ``allow_control``
   * - the owner in person
     - everything
     - the final squash-merge, config, contract, tokens, the kill switches

Specific measures:

- **Force-push with lease only.** The overseer's git wrapper rewrites ``push --force`` to
  ``--force-with-lease=<branch>:<expected sha>`` and refuses a push whose target does not match
  ``[overseer] fork_remotes``. A lease failure (someone else pushed) is reported to the item's thread
  and the item is replanned; nothing is retried blindly.
- **No merge path.** ``fridica-overseer`` has no code that calls the merge endpoint, the merge-queue
  endpoint or ``git push`` to upstream. Its token cannot do it either: at start the overseer reads
  the token's repository permissions (fine-grained) or ``X-OAuth-Scopes`` (classic) and refuses to
  run with ``contents: write`` on an upstream repository or with a classic ``repo`` scope. The
  strongest layer is not the token but branch protection on upstream ``main`` (pushes restricted to
  the owner, pull request required), which holds whatever credentials a process ends up with.
- **Model outputs are proposals.** In the overseer, a model may propose a registry diff or a
  summary; code validates every sha, run id and item id in it against the registry before anything
  is posted or applied. In the daemon, rule P6 keeps the model from asserting state.
- **Prompt injection through PR bodies, issues, attachments and GitHub state** is handled as in F1
  and F3: framed as untrusted data, with the fixture tests carried into the Rust suite. The
  overseer reads the status block for *what to look at*, never for *what to do*.
- **Memory safety** is the language's; the daemon still runs as an unprivileged user and still
  refuses a second daemon on the database.
- **Audit.** Every control API call records ``actor``; every overseer action records its head sha and
  outcome; the daily report reproduces both. An action taken under the wrong clearance is therefore
  a visible event, not a silent one.
