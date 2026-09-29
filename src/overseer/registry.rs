//! Own database and crash-safe effect intents. An executing action becomes
//! uncertain on restart and must be reconciled, never blindly executed twice.
use super::{plan, Item};
use crate::{core::Authority, store::Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Campaign {
    pub id: String,
    pub lead: String,
    pub owners: Vec<String>,
    pub substitutes: Vec<String>,
    pub merge_order: Vec<String>,
}
pub struct Registry {
    pub store: Store,
}
impl Registry {
    pub async fn open(path: PathBuf) -> Result<Self> {
        let store = Store::open_overseer(path).await?;
        store
            .call(|c| {
                c.execute(
                    "UPDATE actions SET state='uncertain' WHERE state='executing'",
                    [],
                )?;
                Ok(())
            })
            .await?;
        Ok(Self { store })
    }
    pub async fn campaign(&self, campaign: Campaign, authority: Authority, now: f64) -> Result<()> {
        if !allowed(&authority, &campaign.id) {
            bail!("campaign rules require an authenticated owner or lead");
        }
        if campaign.lead.is_empty() || campaign.owners.is_empty() {
            bail!("campaign requires lead and owners");
        }
        self.store.call(move|c|{
            let tx=c.transaction()?;
            tx.execute("INSERT INTO campaigns(id,lead,revision,data_json,updated) VALUES(?,?,1,?,?)
                ON CONFLICT(id) DO UPDATE SET lead=excluded.lead,revision=revision+1,data_json=excluded.data_json,updated=excluded.updated",
                params![campaign.id,campaign.lead,serde_json::to_string(&campaign)?,now])?;
            tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,'campaign.rules',?,'{}')",params![now,serde_json::to_string(&authority)?,campaign.id])?;
            tx.commit()?;Ok(())
        }).await
    }
    pub async fn item(
        &self,
        campaign: String,
        item: Item,
        authority: Authority,
        now: f64,
    ) -> Result<()> {
        if !allowed(&authority, &campaign) {
            bail!("item registration requires an authenticated owner or lead");
        }
        self.store.call(move|c|{
            let tx=c.transaction()?;
            let existing:Option<String>=tx.query_row("SELECT campaign_id FROM work_items WHERE id=?",[&item.id],|r|r.get(0)).optional()?;
            if existing.as_ref().is_some_and(|v|v!=&campaign) {bail!("item already belongs to another campaign");}
            tx.execute("INSERT INTO work_items(id,campaign_id,head_sha,head_tree,revision,data_json,updated) VALUES(?,?,?,?,1,?,?)
                 ON CONFLICT(id) DO UPDATE SET head_sha=excluded.head_sha,head_tree=excluded.head_tree,revision=revision+1,data_json=excluded.data_json,updated=excluded.updated",
                params![item.id,campaign,item.head,item.tree,serde_json::to_string(&item)?,now])?;
            tx.execute("UPDATE actions SET state='refused',finished=? WHERE item_id=? AND head_sha!=? AND state='planned'",params![now,item.id,item.head])?;
            let revision:i64=tx.query_row("SELECT revision FROM work_items WHERE id=?",[&item.id],|r|r.get(0))?;
            let payload=json!({"campaign_id":campaign,"item":item,"revision":revision});
            tx.execute("INSERT INTO exchanges(id,kind,payload_json,created) VALUES(?,'mirror',?,?)",params![format!("mirror:{}:{revision}",item.id),payload.to_string(),now])?;
            tx.commit()?;Ok(())
        }).await
    }
    pub async fn plan(&self, item_id: String, id: String, now: f64) -> Result<String> {
        self.store.call(move|c|{
            let tx=c.transaction()?;
            let (data,stopped):(String,bool)=tx.query_row("SELECT w.data_json,c.stopped FROM work_items w JOIN campaigns c ON w.campaign_id=c.id WHERE w.id=?",[&item_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let mut item:Item=serde_json::from_str(&data)?;
            item.stopped|=stopped;
            let decision=serde_json::to_string(&plan(&item))?;
            let existing:Option<String>=tx.query_row("SELECT id FROM actions WHERE item_id=? AND head_sha=? AND action=?",params![item_id,item.head,decision],|r|r.get(0)).optional()?;
            if let Some(id)=existing{return Ok(id);}
            tx.execute("INSERT INTO actions(id,item_id,head_sha,action,state,detail_json,created) VALUES(?,?,?,?,'planned','{}',?)",params![id,item_id,item.head,decision,now])?;
            tx.commit()?;Ok(id)
        }).await
    }
    pub async fn begin(&self, id: String) -> Result<()> {
        self.store.call(move|c|{
            let tx=c.transaction()?;
            let row:Option<(String,String)>=tx.query_row("SELECT w.data_json,a.action FROM actions a JOIN work_items w ON w.id=a.item_id JOIN campaigns c ON c.id=w.campaign_id
                WHERE a.id=? AND a.state='planned' AND a.head_sha=w.head_sha AND c.stopped=0",[&id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((data,planned))=row else {bail!("action is stopped, stale, or already attempted");};
            let item:Item=serde_json::from_str(&data)?;
            if item.owner_paused || item.linked_thread_paused || serde_json::to_string(&plan(&item))?!=planned {bail!("action evidence or pause state changed");}
            tx.execute("UPDATE actions SET state='executing' WHERE id=?",[id])?;
            tx.commit()?;Ok(())
        }).await
    }
    pub async fn reconcile(
        &self,
        id: String,
        outcome: Value,
        verified: bool,
        now: f64,
    ) -> Result<()> {
        if !verified {
            bail!("uncertain outcomes require verified external evidence");
        }
        self.store.call(move|c|{
            if c.execute("UPDATE actions SET state='done',detail_json=?,finished=? WHERE id=? AND state IN ('executing','uncertain')",params![outcome.to_string(),now,id])?!=1 {bail!("action is not awaiting reconciliation");}
            Ok(())
        }).await
    }
    pub async fn stop(&self, campaign: String, authority: Authority, now: f64) -> Result<()> {
        if !allowed(&authority, &campaign) {
            bail!("stop requires owner or lead");
        }
        self.store.call(move|c|{
            let tx=c.transaction()?;
            if tx.execute("UPDATE campaigns SET stopped=1,updated=? WHERE id=?",params![now,campaign])?!=1 {bail!("unknown campaign");}
            tx.execute("UPDATE actions SET state='refused',finished=? WHERE state='planned' AND item_id IN (SELECT id FROM work_items WHERE campaign_id=?)",params![now,campaign])?;
            tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,'campaign.stop',?,'{}')",params![now,serde_json::to_string(&authority)?,campaign])?;
            tx.commit()?;Ok(())
        }).await
    }
    pub async fn acknowledge(&self, id: String, response: Value, now: f64) -> Result<()> {
        self.store.call(move|c|{
            c.execute("UPDATE exchanges SET state='acknowledged',response_json=?,acknowledged_at=? WHERE id=? AND state='pending'",params![response.to_string(),now,id])?;
            Ok(())
        }).await
    }
}
fn allowed(actor: &Authority, campaign: &str) -> bool {
    matches!(actor, Authority::Owner)
        || matches!(actor,Authority::Lead{campaign:id} if id==campaign)
}
