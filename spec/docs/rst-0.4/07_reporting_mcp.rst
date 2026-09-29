.. _reporting:

Daily reports and the desktop integration
=========================================

The report
----------

Once a day, at ``[report] time`` in ``[report] timezone``, the daemon writes one report per
configured channel for the previous local day. A report is generated from the state database only;
it makes one model call at most (the prose summary), and none when ``[report] prose = false``.

.. list-table:: Sections of a channel report
   :header-rows: 1
   :widths: 26 74

   * - Section
     - Content (all from the database; counts link to threads)
   * - Needs you
     - escalated and overdue obligations, oldest first; pending approvals; paused and blocked threads
       with their reasons; work items in ``WaitingForHuman`` (*READY to squash-merge*)
   * - Yesterday
     - threads answered, mentions received / answered / declined (with reasons), jobs run per machine
       with done / failed / interrupted, posts sent, artifacts uploaded, debriefs
   * - Open work
     - work items with state, owner, needs and due; asks with due times, grouped by who owes them
   * - Overseer
     - the overseer's actions: rebases, pushes, reminders, restatements, readiness declarations, refusals
   * - Health
     - Slack connection drops and catch-up gaps, outbox retries and ambiguous posts, parent-call errors
       and latency percentiles, backend errors by kind (R7), doctor warnings
   * - Summary
     - four to eight sentences of prose in the owner's voice, generated from the sections above with
       the same "every number must appear in the data" check as overseer summaries

Reports are stored twice: as a row in ``reports`` (channel, date, JSON of the sections, the Markdown)
and as a file ``<state dir>/reports/<channel-name>/<YYYY-MM-DD>.md``. The file is the durable,
tool-independent form; the row is what the control API serves. With ``post_to_channel = true`` the
Markdown is also posted into the channel as the owner, through the outbox, as kind ``report`` with a
details upload when it exceeds the reply length. Reports older than ``keep_days`` are deleted from
the table, never from the files.

The reporter is a daemon task:

.. code-block:: rust

   pub async fn run(rt: Arc<Runtime>) -> Result<()> {
       loop {
           // 07:00 America/New_York → the next instant
           let next = schedule::next_local(&rt.cfg.report, rt.clock.now());
           rt.clock.sleep_until(next).await;
           let day = LocalDay::yesterday(&rt.cfg.report.timezone, rt.clock.now());
           for channel in rt.cfg.slack.channels.iter() {
               if rt.store.report_exists(channel, day).await? { continue }   // idempotent across restarts
               let data = collector::collect(&rt.store, channel, day).await?;   // the sections, as data
               let prose = match rt.cfg.report.prose {
                   true => summarizer::write(&rt.parent, &data).await.ok(),
                   false => None,
               };
               let md = render::markdown(&data, prose.as_deref());
               rt.store.save_report(channel, day, &data, &md).await?;          // row + file, one step
               if rt.cfg.report.post_to_channel {
                   rt.store.enqueue_post(Post::report(channel, &md)).await?;
               }
           }
       }
   }

``fridica report [--channel NAME] [--date YYYY-MM-DD] [--regenerate]`` prints a report or rebuilds it;
the dashboard's *Reports* view lists them.

The MCP server
--------------

Claude Desktop and Codex Desktop both speak the Model Context Protocol to local servers over stdio.
``fridica mcp`` is such a server. It is a thin client of the daemon's control socket: it holds no
database, no Slack token and no GitHub token, and it does nothing the CLI could not do.

.. list-table:: MCP surface
   :header-rows: 1
   :widths: 26 14 60

   * - Name
     - Kind
     - Behaviour
   * - ``fridica://reports/{channel}/{date}``
     - resource
     - the Markdown report; ``latest`` as date
   * - ``fridica://threads/{id}``
     - resource
     - the thread's history view (what the dashboard shows), obligations and jobs
   * - ``list_channels``
     - tool
     - configured channels with names and today's report availability
   * - ``daily_report(channel, date?)``
     - tool
     - the report's sections as JSON plus the Markdown
   * - ``attention()``
     - tool
     - escalated and overdue obligations, pending approvals, paused and blocked threads, items waiting
       for the owner; the same list the dashboard shows
   * - ``obligations(thread?, state?)``
     - tool
     - obligations with filters
   * - ``work_items(repo?, state?)``
     - tool
     - the overseer's registry, read through the daemon (the overseer mirrors its registry to the
       daemon's ``work_items`` table)
   * - ``thread(id)``
     - tool
     - one thread in full
   * - ``close_obligation(id, reason)``
     - tool (control)
     - closes an obligation as the owner
   * - ``resume_thread(id)``, ``pause_thread(id)``, ``instruct_thread(id, text)``
     - tool (control)
     - the dashboard's owner actions
   * - ``decide_approval(id, once|session|deny)``
     - tool (control)
     - answers a pending approval
   * - ``register_work_item(...)``
     - tool (control)
     - adds an item to the overseer's campaign

Control tools exist only when ``[mcp] allow_control = true``; otherwise the server does not advertise
them. Every control call carries ``actor = owner-desktop`` into the audit table.

Registration:

.. code-block:: json

   // Claude Desktop: claude_desktop_config.json
   {
     "mcpServers": {
       "fridica": { "command": "fridica", "args": ["mcp"], "env": { "FRIDICA_MCP_KEY": "…" } }
     }
   }

.. code-block:: toml

   # Codex Desktop / CLI: ~/.codex/config.toml
   [mcp_servers.fridica]
   command = "fridica"
   args = ["mcp"]
   env = { FRIDICA_MCP_KEY = "…" }

``fridica mcp --print-config`` prints both snippets with the key filled in. ``fridica init`` creates
the key in ``<state dir>/mcp.key`` (mode 0600).

Keeping the MCP server away from workers
----------------------------------------

There is one trap here, and the 0.3 doctor already warns about its shape: ``codex app-server`` loads
the machine owner's ``~/.codex/config.toml``, so a Codex *worker* on the owner's machine would start
``fridica mcp`` too and could read the owner's threads and, with control tools, steer them. Three
measures close it:

1. ``fridica mcp`` requires ``FRIDICA_MCP_KEY`` to match ``mcp.key`` before it advertises any tool or
   resource. The daemon removes ``FRIDICA_MCP_KEY`` from every worker's environment
   (``Config.secret_env``, where the Slack and GitHub tokens already are). A worker's Codex therefore
   starts a server that exposes nothing.
2. The daemon sets ``FRIDICA_WORKER=1`` in every worker's environment; ``fridica mcp`` exits at once
   when it sees it, before touching the socket.
3. ``doctor`` reports the ``fridica`` entry in ``~/.codex/config.toml`` on machines that run Codex
   workers, as it reports other MCP servers today, and recommends a Codex profile for the desktop
   (``[profiles.desktop.mcp_servers.fridica]``) so that ``codex app-server`` started by the daemon,
   which passes no profile, does not load it at all.

What the desktop agent can do with it
-------------------------------------

The report answers "what happened and what needs me"; the tools let the desktop agent act on the
answer without opening Slack: close an obligation that is done, resume a thread with an instruction,
answer an approval, register the next PR in the campaign. The desktop agent is a person's assistant,
not another Fridica: it never posts to Slack (there is no tool for it), so the "one automated voice
per channel" rule that made the campaign work holds.
