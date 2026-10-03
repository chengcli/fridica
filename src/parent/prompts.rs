pub use crate::config::prompts::UNTRUSTED;
use crate::config::prompts::{ACTION, DEBRIEF, TRIAGE};
use crate::{
    config::{contract, repos, Config},
    core::parent::ParentRequest,
};
use anyhow::{bail, Result};
use serde_json::json;

pub fn build(
    config: &Config,
    request: &ParentRequest,
) -> Result<(String, serde_json::Value, String)> {
    let rules = contract::load(config.owner.contract.as_deref())?;
    let repositories = repos::load(config.parent.repos.as_deref())?;
    let (instructions, schema, model) = match request.call.as_str() {
        "triage" => (
            format!("{}\n{TRIAGE}", rules.participation),
            super::schema::triage(),
            if config.parent.triage_model.is_empty() {
                &config.parent.model
            } else {
                &config.parent.triage_model
            },
        ),
        "decide" | "repair" => (
            format!("{}\n\n{ACTION}\n", rules.parent()),
            super::schema::decision(&super::schema::Choices::from_session(&request.session)),
            &config.parent.model,
        ),
        "debrief" => (
            format!("{}\n{}\n{DEBRIEF}", rules.debriefs, rules.extra),
            super::schema::debrief(),
            &config.parent.model,
        ),
        _ => bail!("unsupported parent call"),
    };
    let (mut history, trigger) = super::context::prepare(
        &request.history,
        &request.trigger,
        config.parent.context_chars,
    );
    if request.call == "triage" && history.len() > 15 {
        history.drain(..history.len() - 15);
    }
    let mut session = request.session.clone();
    let channel = session
        .as_object_mut()
        .and_then(|s| s.remove("channel_context"));
    let linked_threads = session
        .as_object_mut()
        .and_then(|s| s.remove("linked_threads"));
    let mut data = json!({"now":request.session["now"],"owner_id":config.owner.slack_user,"profile":config.owner.profile,"repositories":repositories,
        "session":session,"trigger":trigger,"history":history,"obligations":request.obligations,"linked":request.linked,
        "machines":request.session["machines"],"workers":request.session["work"]["workers"],
        "delegation_allowed":request.session["channel"].as_str().is_some_and(|c|config.slack.may_delegate(c)),
        "limits":{"reply_chars":config.limits.reply_chars,"max_delegations":config.limits.max_delegations_per_turn,"max_workers":config.limits.max_workers_per_thread},
        "repair":{"errors":request.errors,"previous_answer":request.previous}});
    if request.call != "triage" && !request.github_state.is_empty() {
        data["github_state"] = json!(request.github_state);
    }
    if matches!(request.call.as_str(), "decide" | "repair") {
        if let Some(channel) = channel.filter(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
            data["channel_context"] = channel;
        }
        if let Some(linked) = linked_threads.filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
        {
            data["linked_threads"] = linked;
        }
    }
    Ok((
        format!(
            "{}\n\n{instructions}\n{UNTRUSTED}\n\nData:\n{}",
            crate::config::provisions::shared(),
            serde_json::to_string(&data)?
        ),
        schema,
        model.clone(),
    ))
}
