//! A small, validated presentation language. The model never writes native Feishu JSON.
use crate::{
    config::Config,
    store::{Job, Store},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Debug)]
pub struct FormInputError(pub String);
impl std::fmt::Display for FormInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for FormInputError {}
pub const INSTRUCTIONS: &str = include_str!("../resource/feishu-replies.txt");
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub feishu_reply: u8,
    pub text: String,
    #[serde(default)]
    pub layout: Layout,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default)]
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub select: Option<Select>,
    #[serde(default)]
    pub form: Option<Form>,
    #[serde(default)]
    pub media: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    Text,
    Card,
    Post,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Success,
    Warning,
    Error,
}
impl Tone {
    fn color(self) -> &'static str {
        match self {
            Self::Neutral => "blue",
            Self::Success => "green",
            Self::Warning => "yellow",
            Self::Error => "red",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    Markdown {
        text: String,
    },
    Fields {
        items: Vec<FieldValue>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Details {
        title: String,
        text: String,
    },
    Image {
        task_id: String,
        #[serde(default)]
        caption: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldValue {
    pub label: String,
    pub value: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub url: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub prompt: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    pub label: String,
    pub options: Vec<Choice>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Form {
    pub prompt: String,
    pub fields: Vec<Input>,
    #[serde(default)]
    pub submit_label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub id: String,
    pub label: String,
    pub kind: InputKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<Choice>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Text,
    Multiline,
    Integer,
    Number,
    SingleSelect,
    MultiSelect,
    Boolean,
    Date,
    Time,
}
fn text(s: &str, min: usize, max: usize) -> Result<()> {
    ensure!(
        !s.contains('\0')
            && (min..=max).contains(&s.chars().count())
            && (min == 0 || !s.trim().is_empty()),
        "文字长度或内容无效"
    );
    Ok(())
}
fn id(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 32 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "交互标识只允许32位以内字母、数字、下划线"
    );
    Ok(())
}
fn prompt(s: &str) -> Result<()> {
    text(s, 1, 2000)?;
    ensure!(
        !s.trim_start().starts_with('/'),
        "卡片操作应是自然语言请求，不能执行slash授权或切换命令"
    );
    Ok(())
}
fn url(s: &str) -> Result<()> {
    let u = reqwest::Url::parse(s)?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none(),
        "链接须为无内嵌凭证的HTTP(S)地址"
    );
    Ok(())
}
fn choices(options: &[Choice], followup: bool) -> Result<()> {
    ensure!((1..=20).contains(&options.len()), "选项支持1–20项");
    let mut ids = HashSet::new();
    for o in options {
        id(&o.id)?;
        ensure!(ids.insert(&o.id), "选项标识重复");
        text(&o.label, 1, 80)?;
        if followup {
            prompt(&o.prompt)?;
        } else {
            ensure!(o.prompt.is_empty(), "表单选项不接受执行指令");
        }
    }
    Ok(())
}
impl Reply {
    pub fn interactive(&self) -> bool {
        !self.actions.is_empty() || self.select.is_some() || self.form.is_some()
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.feishu_reply == 1, "未知回复协议版本");
        text(&self.text, 1, 12000)?;
        text(&self.title, 0, 80)?;
        ensure!(
            self.blocks.len() <= 5 && self.actions.len() <= 6 && self.media.len() <= 4,
            "回复组件过多"
        );
        ensure!(
            self.layout == Layout::Card || !self.interactive(),
            "交互组件必须使用card布局"
        );
        ensure!(
            self.layout != Layout::Text || self.blocks.is_empty(),
            "text布局不能包含展示块"
        );
        let action_groups =
            if self.actions.len() > 3 && self.actions.iter().any(|a| !a.url.is_empty()) {
                2
            } else {
                usize::from(!self.actions.is_empty())
            };
        ensure!(
            self.layout != Layout::Card
                || 1 + self.blocks.len()
                    + action_groups
                    + usize::from(self.select.is_some())
                    + 2 * usize::from(self.form.is_some())
                    <= 5,
            "卡片视觉区块过多"
        );
        ensure!(
            self.form.is_none() || (self.actions.is_empty() && self.select.is_none()),
            "表单应独立提交，不与其他操作混用"
        );
        let mut ids = HashSet::new();
        for a in &self.actions {
            id(&a.id)?;
            ensure!(
                !matches!(a.id.as_str(), "select" | "more"),
                "操作标识为保留名"
            );
            ensure!(ids.insert(&a.id), "操作标识重复");
            text(&a.label, 1, 80)?;
            ensure!(
                a.url.is_empty() != a.prompt.is_empty(),
                "按钮必须选择回调请求或链接之一"
            );
            if a.url.is_empty() {
                prompt(&a.prompt)?
            } else {
                url(&a.url)?
            }
        }
        if let Some(s) = &self.select {
            text(&s.label, 1, 80)?;
            choices(&s.options, true)?;
        }
        if let Some(f) = &self.form {
            prompt(&f.prompt)?;
            text(&f.submit_label, 0, 40)?;
            ensure!((1..=8).contains(&f.fields.len()), "表单支持1–8项");
            let mut ids = HashSet::new();
            for field in &f.fields {
                id(&field.id)?;
                ensure!(
                    ids.insert(&field.id) && !field.id.starts_with("reply_"),
                    "表单标识重复或为保留名"
                );
                text(&field.label, 1, 80)?;
                if matches!(field.kind, InputKind::SingleSelect | InputKind::MultiSelect) {
                    choices(&field.options, false)?;
                } else {
                    ensure!(field.options.is_empty(), "非选择项不接受options");
                }
                ensure!(
                    field.min.is_none_or(f64::is_finite)
                        && field.max.is_none_or(f64::is_finite)
                        && field.min.zip(field.max).is_none_or(|(a, b)| a <= b),
                    "数字范围无效"
                );
                ensure!(
                    matches!(field.kind, InputKind::Integer | InputKind::Number)
                        || (field.min.is_none() && field.max.is_none()),
                    "仅数字字段支持范围"
                );
            }
        }
        for block in &self.blocks {
            match block {
                Block::Markdown { text: s } => text(s, 1, 4000)?,
                Block::Details { title, text: s } => {
                    text(title, 1, 80)?;
                    text(s, 1, 4000)?;
                    ensure!(self.layout == Layout::Card, "折叠详情仅用于card");
                }
                Block::Fields { items } => {
                    ensure!((1..=8).contains(&items.len()), "字段对支持1–8项");
                    for f in items {
                        text(&f.label, 1, 80)?;
                        text(&f.value, 1, 500)?;
                    }
                }
                Block::Table { columns, rows } => {
                    ensure!(
                        (1..=6).contains(&columns.len()) && rows.len() <= 30,
                        "表格最多6列30行"
                    );
                    for c in columns {
                        text(c, 1, 80)?;
                    }
                    for row in rows {
                        ensure!(row.len() == columns.len(), "表格列数不匹配");
                        for c in row {
                            text(c, 0, 300)?;
                        }
                    }
                }
                Block::Image { task_id, caption } => {
                    text(task_id, 1, 200)?;
                    text(caption, 0, 200)?;
                }
            }
        }
        Ok(())
    }
}
/// Only a complete envelope is interpreted, never JSON/code embedded in a normal explanation.
pub fn parse(raw: &str) -> Option<Result<Reply>> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("```feishu\n")
        .or_else(|| raw.strip_prefix("```json\n"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(raw)
        .trim();
    if !raw.starts_with('{') || !raw.contains("\"feishu_reply\"") {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw)
        && v.get("feishu_reply").is_none()
    {
        return None;
    }
    Some((|| {
        let r: Reply = serde_json::from_str(raw)?;
        r.validate()?;
        Ok(r)
    })())
}
pub fn fallback(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_start_matches("```feishu")
        .trim_start_matches("```json")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(s) = v["text"].as_str()
        && text(s, 1, 12000).is_ok()
    {
        return s.into();
    }
    "这条回复的展示格式出了问题，请再试一次。".into()
}
fn md(s: &str) -> String {
    // Mentions and embedded images must not bypass typed recipients/assets.
    s.replace('<', "&lt;").replace("![", "\\![")
}
fn plain(s: &str) -> Value {
    json!({"tag":"plain_text","content":s})
}
fn markdown(s: &str) -> Value {
    json!({"tag":"markdown","content":md(s)})
}
fn callback(token: &str, id: &str) -> Value {
    json!([{"type":"callback","value":{"action":"reply_action","token":token,"id":id}}])
}
fn button(a: &Action, token: &str, primary: bool) -> Value {
    let mut b = json!({"tag":"button","text":plain(&a.label),"type":if primary{"primary_filled"}else{"default"}});
    b["behaviors"] = if a.url.is_empty() {
        callback(token, &a.id)
    } else {
        json!([{"type":"open_url","default_url":a.url}])
    };
    b
}
fn options(choices: &[Choice]) -> Value {
    json!(
        choices
            .iter()
            .map(|o| json!({"text":plain(&o.label),"value":o.id}))
            .collect::<Vec<_>>()
    )
}
fn media(store: &Store, key: &str, task: &str) -> Result<Value> {
    let row = store
        .command_task(key, task)?
        .context("素材不存在或不属于当前会话")?;
    ensure!(
        row["kind"] == "camera_media"
            && matches!(row["status"].as_str(), Some("replying" | "done")),
        "素材尚未准备好"
    );
    let payload: Value = serde_json::from_str(row["result"].as_str().context("素材内容缺失")?)?;
    ensure!(
        matches!(
            payload["msg_type"].as_str(),
            Some("image" | "post" | "media")
        ),
        "不支持的素材类型"
    );
    Ok(payload)
}
fn image_key(store: &Store, key: &str, task: &str) -> Result<String> {
    let p = media(store, key, task)?;
    if let Some(k) = p["content"]["image_key"].as_str() {
        return Ok(k.into());
    }
    if let Some(rows) = p["content"]["zh_cn"]["content"].as_array() {
        for row in rows {
            if let Some(tags) = row.as_array() {
                for tag in tags {
                    if tag["tag"] == "img"
                        && let Some(k) = tag["image_key"].as_str()
                    {
                        return Ok(k.into());
                    }
                }
            }
        }
    }
    bail!("素材没有可用图片")
}
fn input(field: &Input) -> Value {
    let mut v = match field.kind {
        InputKind::Text | InputKind::Multiline | InputKind::Integer | InputKind::Number => {
            json!({"tag":"input","input_type":if field.kind==InputKind::Multiline{"multiline_text"}else{"text"},"max_length":1000})
        }
        InputKind::SingleSelect | InputKind::MultiSelect => {
            json!({"tag":if field.kind==InputKind::SingleSelect{"select_static"}else{"multi_select_static"},"options":options(&field.options)})
        }
        InputKind::Boolean => {
            json!({"tag":"select_static","options":[{"text":plain("是"),"value":"true"},{"text":plain("否"),"value":"false"}]})
        }
        InputKind::Date => json!({"tag":"date_picker"}),
        InputKind::Time => json!({"tag":"picker_time"}),
    };
    v["name"] = json!(field.id);
    v["required"] = json!(field.required);
    v["width"] = json!("fill");
    v["placeholder"] = plain(&field.label);
    v
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Packet {
    pub kind: String,
    pub content: Value,
    #[serde(default)]
    pub token: String,
}
#[derive(Serialize, Deserialize)]
pub struct Output {
    pub fallback: String,
    pub packets: Vec<Packet>,
}
/// Produce native payloads before saving an outbox item. No network or sends here.
pub fn render(r: &Reply, store: &Store, key: &str, token: &str) -> Result<Output> {
    r.validate()?;
    let mut packets = Vec::new();
    match r.layout {
        Layout::Text => {
            for part in crate::bridge::chunks(&r.text) {
                packets.push(Packet {
                    kind: "text".into(),
                    content: json!({"text":part}),
                    token: String::new(),
                });
            }
        }
        Layout::Post => {
            let mut rows = vec![json!([{"tag":"text","text":r.text}])];
            for block in &r.blocks {
                match block {
                    Block::Image { task_id, caption } => {
                        if !caption.is_empty() {
                            rows.push(json!([{"tag":"text","text":caption}]));
                        }
                        rows.push(json!([{"tag":"img","image_key":image_key(store,key,task_id)?}]));
                    }
                    Block::Markdown { text: s } => rows.push(json!([{"tag":"text","text":s}])),
                    Block::Fields { items } => {
                        for item in items {
                            rows.push(json!([{"tag":"text","text":format!("{}：{}",item.label,item.value)}]));
                        }
                    }
                    Block::Table {
                        columns,
                        rows: table,
                    } => {
                        rows.push(json!([{"tag":"text","text":columns.join(" · ")}]));
                        for row in table {
                            rows.push(json!([{"tag":"text","text":row.join(" · ")}]));
                        }
                    }
                    Block::Details { .. } => bail!("post不支持折叠详情"),
                }
            }
            packets.push(Packet {
                kind: "post".into(),
                content: json!({"zh_cn":{"title":r.title,"content":rows}}),
                token: String::new(),
            });
        }
        Layout::Card => {
            let mut elements = vec![
                json!({"tag":"column_set","flex_mode":"none","background_style":format!("{}-50",r.tone.color()),"columns":[{"tag":"column","width":"weighted","weight":1,"elements":[markdown(&r.text)]}]}),
            ];
            for block in &r.blocks {
                elements.push(match block {
                Block::Markdown{text:s}=>markdown(s),
                Block::Fields{items}=>json!({"tag":"column_set","flex_mode":"none","columns":items.chunks(4).map(|chunk|json!({"tag":"column","width":"weighted","weight":1,"elements":chunk.iter().map(|f|markdown(&format!("**{}**\n{}",f.label,f.value))).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
                Block::Details{title,text:s}=>json!({"tag":"collapsible_panel","expanded":false,"header":{"title":plain(title)},"elements":[markdown(s)]}),
                Block::Image{task_id,caption}=>json!({"tag":"img","img_key":image_key(store,key,task_id)?,"alt":plain(caption),"scale_type":"fit_horizontal","preview":true}),
                Block::Table{columns,rows}=>json!({"tag":"table","page_size":5,"columns":columns.iter().enumerate().map(|(i,c)|json!({"name":format!("c{i}"),"display_name":c,"data_type":"text"})).collect::<Vec<_>>(),"rows":rows.iter().map(|row|row.iter().enumerate().map(|(i,c)|(format!("c{i}"),json!(c))).collect::<serde_json::Map<String,Value>>()).collect::<Vec<_>>()})
            });
            }
            if let Some(s) = &r.select {
                elements.push(json!({"tag":"select_static","name":"reply_select","width":"fill","placeholder":plain(&s.label),"options":options(&s.options),"behaviors":callback(token,"select")}));
            }
            if let Some(f) = &r.form {
                let mut fields = Vec::new();
                for field in &f.fields {
                    fields.push(markdown(&format!(
                        "{}{}",
                        field.label,
                        if field.required { "（必填）" } else { "" }
                    )));
                    fields.push(input(field));
                }
                // Form submit callbacks are routed by the bound message ID, not an arbitrary action payload.
                fields.push(json!({"tag":"button","name":"reply_submit","type":"primary_filled","width":"fill","form_action_type":"submit","text":plain(if f.submit_label.is_empty(){"提交"}else{&f.submit_label})}));
                fields.push(json!({"tag":"button","name":"reply_reset","type":"default","form_action_type":"reset","text":plain("重新填写")}));
                elements.push(json!({"tag":"form","name":"reply_form","elements":fields}));
                elements.push(json!({"tag":"button","text":plain("取消"),"type":"default","behaviors":callback(token,"cancel")}));
            }
            if !r.actions.is_empty() {
                let fold = r.actions.len() > 3 && r.actions.iter().all(|a| a.url.is_empty());
                let visible = if fold { 2 } else { r.actions.len().min(3) };
                let mut columns = r.actions[..visible]
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let mut b = button(a, token, i == 0);
                        if r.actions.len() == 1 {
                            b["width"] = json!("fill");
                        }
                        json!({"tag":"column","width":"weighted","weight":1,"elements":[b]})
                    })
                    .collect::<Vec<_>>();
                if fold {
                    let options = r.actions[visible..]
                        .iter()
                        .map(|a| {
                            let mut o = json!({"text":plain(&a.label),"value":a.id});
                            if !a.url.is_empty() {
                                o["multi_url"] = json!({"url":a.url});
                            }
                            o
                        })
                        .collect::<Vec<_>>();
                    columns.push(json!({"tag":"column","width":"weighted","weight":1,"elements":[{"tag":"overflow","options":options,"behaviors":callback(token,"more") }]}));
                }
                elements.push(json!({"tag":"column_set","flex_mode":"none","columns":columns}));
                if !fold && visible < r.actions.len() {
                    let columns=r.actions[visible..].iter().map(|a|json!({"tag":"column","width":"weighted","weight":1,"elements":[button(a,token,false)]})).collect::<Vec<_>>();
                    elements.push(json!({"tag":"column_set","flex_mode":"none","columns":columns}));
                }
            }
            let card = json!({"schema":"2.0","config":{"update_multi":true,"enable_forward":false,"width_mode":"default"},"header":{"title":plain(if r.title.is_empty(){"李老师"}else{&r.title}),"template":r.tone.color(),"icon":{"tag":"standard_icon","token":"notice_colorful"}},"body":{"vertical_spacing":"large","elements":elements}});
            ensure!(card.to_string().len() <= 24000, "卡片内容过长");
            packets.push(Packet {
                kind: "interactive".into(),
                content: card,
                token: if r.interactive() {
                    token.into()
                } else {
                    String::new()
                },
            });
        }
    }
    for task in &r.media {
        let p = media(store, key, task)?;
        packets.push(Packet {
            kind: p["msg_type"].as_str().unwrap().into(),
            content: p["content"].clone(),
            token: String::new(),
        });
    }
    Ok(Output {
        fallback: r.text.clone(),
        packets,
    })
}

pub fn finish(store: &Store, j: &Job, raw: &str) -> Result<()> {
    match parse(raw) {
        None => store.finish(&j.id, raw),
        Some(result) => {
            let rendered: Result<Output> = (|| {
                let r = result?;
                let token = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_OID,
                    format!("reply-card:{}", j.id).as_bytes(),
                )
                .simple()
                .to_string();
                let output = render(&r, store, &j.key, &token)?;
                if r.interactive() {
                    store.save_reply_card(&token, &j.key, &serde_json::to_value(&r)?)?;
                }
                Ok(output)
            })();
            match rendered {
                Ok(out) => store.finish_output(&j.id, "rich_reply", &serde_json::to_value(out)?),
                Err(e) => {
                    tracing::warn!(error=%e,"Agent回复结构无效，使用正文降级");
                    store.finish(&j.id, &fallback(raw))
                }
            }
        }
    }
}
fn answers(form: &Form, raw: &Value) -> Result<Value> {
    let object = raw.as_object().context("表单答案缺失")?;
    ensure!(object.len() <= form.fields.len() + 2, "表单字段过多");
    ensure!(
        object.keys().all(|k| form.fields.iter().any(|f| f.id == *k)
            || matches!(k.as_str(), "reply_submit" | "reply_reset")),
        "表单包含未知字段"
    );
    let mut out = serde_json::Map::new();
    for f in &form.fields {
        let v = &raw[&f.id];
        let empty = v.is_null()
            || v.as_str().is_some_and(|s| s.trim().is_empty())
            || v.as_array().is_some_and(Vec::is_empty);
        if empty {
            ensure!(!f.required, "请填写：{}", f.label);
            continue;
        }
        let s = v.as_str().unwrap_or("");
        let valid_option = |value: &str| f.options.iter().any(|o| o.id == value);
        let value = match f.kind {
            InputKind::Text | InputKind::Multiline => {
                ensure!(v.is_string(), "请输入文字");
                text(s, 0, 1000)?;
                json!(s)
            }
            InputKind::Integer => {
                let n = if v.is_string() {
                    s.trim().parse::<i64>().context("请输入整数")?
                } else {
                    v.as_i64().context("请输入整数")?
                };
                let number = n as f64;
                ensure!(
                    f.min.is_none_or(|m| number >= m) && f.max.is_none_or(|m| number <= m),
                    "整数超出范围"
                );
                json!(n)
            }
            InputKind::Number => {
                let n = if v.is_string() {
                    s.trim().parse::<f64>().context("请输入数字")?
                } else {
                    v.as_f64().context("请输入数字")?
                };
                ensure!(
                    n.is_finite() && f.min.is_none_or(|m| n >= m) && f.max.is_none_or(|m| n <= m),
                    "数字超出范围"
                );
                json!(n)
            }
            InputKind::Boolean => json!(if let Some(b) = v.as_bool() {
                b
            } else {
                s.parse::<bool>().context("请选择是或否")?
            }),
            InputKind::SingleSelect => {
                ensure!(valid_option(s), "未知选项");
                json!(s)
            }
            InputKind::MultiSelect => {
                let values = v.as_array().context("请选择多个选项")?;
                ensure!(values.len() <= f.options.len(), "多选数量过多");
                let mut seen = HashSet::new();
                for item in values {
                    let s = item.as_str().context("未知多选项")?;
                    ensure!(valid_option(s) && seen.insert(s), "未知或重复的多选项");
                }
                v.clone()
            }
            InputKind::Date => {
                let date = s.split_whitespace().next().unwrap_or("");
                let date =
                    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").context("日期无效")?;
                json!(date.format("%Y-%m-%d").to_string())
            }
            InputKind::Time => {
                let time = s.split_whitespace().next().unwrap_or("");
                let time = chrono::NaiveTime::parse_from_str(time, "%H:%M")
                    .or_else(|_| chrono::NaiveTime::parse_from_str(time, "%H:%M:%S"))
                    .context("时间无效")?;
                json!(time.format("%H:%M").to_string())
            }
        };
        out.insert(f.id.clone(), value);
    }
    Ok(Value::Object(out))
}
/// Feishu callbacks are trusted only after matching the persisted owner, chat, card and choices.
pub fn parse_action(v: &Value, cfg: &Config, store: &Store) -> Result<Option<Job>> {
    if v["header"]["event_type"] != "card.action.trigger" {
        return Ok(None);
    }
    let event = &v["event"];
    let a = &event["action"];
    let explicit = a["value"]["action"] == "reply_action";
    let form_submit = !explicit && a["tag"] == "button" && a["form_value"].is_object();
    if !explicit && !form_submit {
        return Ok(None);
    }
    let message = event["context"]["open_message_id"].as_str().unwrap_or("");
    let token = if explicit {
        a["value"]["token"].as_str().context("卡片标识缺失")?
    } else {
        ""
    };
    let Some(card) = store.reply_card(token, message)? else {
        if explicit {
            bail!("卡片已过期或失效，请重新发起请求");
        }
        return Ok(None);
    };
    let owner = event["operator"]["open_id"].as_str().unwrap_or("");
    let chat = event["context"]["open_chat_id"].as_str().unwrap_or("");
    let key: Vec<String> = serde_json::from_str(&card.origin_key)?;
    ensure!(
        key.len() == 4
            && key[0] == cfg.workspace.to_string_lossy()
            && key[1] == owner
            && key[2] == chat
            && cfg.allowed.iter().any(|u| u == owner)
            && card.message_id == message,
        "卡片不属于当前用户或聊天"
    );
    ensure!(
        key[3].is_empty()
            || cfg.allowed_chats.is_empty()
            || cfg.allowed_chats.iter().any(|c| c == chat),
        "聊天无权限"
    );
    ensure!(a.to_string().len() <= 32 * 1024, "卡片提交内容过长");
    let spec: Reply = serde_json::from_value(card.spec)?;
    spec.validate()?;
    let (request, cancel) = if form_submit {
        ensure!(!explicit, "回调类型不一致");
        let form = spec.form.as_ref().context("该卡片没有表单")?;
        let values = answers(form, &a["form_value"]).map_err(|e| FormInputError(e.to_string()))?;
        let timezone = a.get("timezone").cloned().unwrap_or(json!("Asia/Shanghai"));
        ensure!(timezone.to_string().len() <= 100, "时区信息无效");
        (
            format!(
                "用户提交了上一条卡片的表单。\n请求：{}\n字段定义（仅用于理解答案）：{}\n用户答案：{}\n界面时区：{}\n原始填写值（日期/时间如带偏移请据此核对）：{}\n按用户提交的信息继续处理；提交不代表任何操作已经执行。",
                form.prompt,
                serde_json::to_string(&form.fields)?,
                values,
                timezone,
                a["form_value"]
            ),
            false,
        )
    } else {
        let selected = a["value"]["id"].as_str().context("缺少操作标识")?;
        if selected == "cancel" && spec.form.is_some() {
            ensure!(a["tag"] == "button", "取消操作无效");
            (String::new(), true)
        } else if selected == "select" {
            ensure!(a["tag"] == "select_static", "选择类型不一致");
            let option = a["option"].as_str().context("缺少选项")?;
            let select = spec.select.as_ref().context("该卡片没有下拉选项")?;
            let choice = select
                .options
                .iter()
                .find(|o| o.id == option)
                .context("未知选项")?;
            (
                format!(
                    "用户在上一条卡片中选择了「{}」。\n请求：{}",
                    choice.label, choice.prompt
                ),
                false,
            )
        } else {
            let selected = if selected == "more" {
                ensure!(a["tag"] == "overflow", "菜单类型不一致");
                a["option"].as_str().context("缺少菜单选项")?
            } else {
                ensure!(a["tag"] == "button", "按钮类型不一致");
                selected
            };
            let action = spec
                .actions
                .iter()
                .find(|b| b.id == selected && b.url.is_empty())
                .context("未知按钮操作")?;
            (
                format!(
                    "用户点击了上一条卡片中的「{}」。\n请求：{}",
                    action.label, action.prompt
                ),
                false,
            )
        }
    };
    let event_id = v["header"]["event_id"]
        .as_str()
        .context("回调事件缺少标识")?;
    text(event_id, 1, 200)?;
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, event_id.as_bytes());
    let job = Job {
        id: format!("reply-action:{uuid}"),
        key: card.origin_key,
        message_id: message.into(),
        text: if cancel {
            "/_reply_cancel".into()
        } else {
            format!("{request}\n\n原卡片说明：{}", spec.text)
        },
        result: String::new(),
        sent: 0,
    };
    ensure!(job.text.len() <= 64 * 1024, "表单内容过长");
    store.consume_reply_card(&card.token, &job)?;
    Ok(Some(job))
}
pub async fn acknowledge(bridge: &crate::bridge::Bridge, j: &Job) -> Result<()> {
    if !j.id.starts_with("reply-action:") {
        return Ok(());
    }
    let Some(card) = bridge.store.reply_card_for_job(&j.id)? else {
        return Ok(());
    };
    ensure!(card.used_job == j.id, "卡片操作记录不一致");
    let r: Reply = serde_json::from_value(card.spec)?;
    let message = if j.text == "/_reply_cancel" {
        "已取消这次填写。"
    } else {
        "已收到，正在处理你的选择。"
    };
    let card = crate::forms::status(
        if r.title.is_empty() {
            "李老师"
        } else {
            &r.title
        },
        &format!("{}\n\n{message}", r.text),
    );
    bridge.feishu.update_card(&j.message_id, &card).await
}

/// Local debugging/verification only. Renders to JSON without calling Feishu or a model.
pub async fn check_stdio() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= 64 * 1024, "回复超过64KB");
    let raw = String::from_utf8(bytes)?;
    let out = if let Some(reply) = parse(&raw) {
        render(
            &reply?,
            &Store::open(std::path::Path::new(":memory:"))?,
            "preview",
            "preview",
        )?
    } else {
        Output {
            fallback: raw.clone(),
            packets: crate::bridge::chunks(&raw)
                .into_iter()
                .map(|s| Packet {
                    kind: "text".into(),
                    content: json!({"text":s}),
                    token: String::new(),
                })
                .collect(),
        }
    };
    tokio::io::stdout()
        .write_all(format!("{}\n", serde_json::to_string(&out)?).as_bytes())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(cfg: &Config) -> Job {
        Job {
            id: "om_parent".into(),
            key: json!([cfg.workspace, "ou_owner", "oc_chat", "root"]).to_string(),
            message_id: "om_parent".into(),
            text: "看看作业".into(),
            result: String::new(),
            sent: 0,
        }
    }
    fn spec() -> Reply {
        serde_json::from_value(json!({"feishu_reply":1,"layout":"card","title":"宝宝的任务","text":"数学还没开始，要提醒一下吗？","actions":[{"id":"say","label":"播报提醒","prompt":"请通过喇叭提醒宝宝写数学作业。"},{"id":"cancel","label":"暂不提醒","prompt":"暂时不用提醒宝宝。"}]})).unwrap()
    }
    fn event(token: &str) -> Value {
        json!({"header":{"event_type":"card.action.trigger","event_id":"evt_one"},"event":{"operator":{"open_id":"ou_owner"},"context":{"open_chat_id":"oc_chat","open_message_id":"om_card"},"action":{"tag":"button","value":{"action":"reply_action","token":token,"id":"say"}}}})
    }
    fn prepare(store: &Store, j: &Job, r: &Reply) -> Result<()> {
        store.save_reply_card("token", &j.key, &serde_json::to_value(r)?)?;
        store.attach_reply_card("token", "om_card")
    }
    #[test]
    fn render_common_displays_and_safe_links_without_native_json_from_model() -> Result<()> {
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let cfg = Config::fixture();
        let j = job(&cfg);
        let mut r = spec();
        r.blocks = vec![
            Block::Fields {
                items: vec![FieldValue {
                    label: "平板".into(),
                    value: "在线".into(),
                }],
            },
            Block::Table {
                columns: vec!["任务".into(), "状态".into()],
                rows: vec![vec!["数学".into(), "未开始".into()]],
            },
            Block::Details {
                title: "详细记录".into(),
                text: "今天的详细记录".into(),
            },
        ];
        r.actions.push(Action {
            id: "link".into(),
            label: "打开网站".into(),
            url: "https://cyeam.com".into(),
            prompt: String::new(),
        });
        let out = render(&r, &store, &j.key, "token")?;
        let card = &out.packets[0].content;
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["body"]["elements"][0]["tag"], "column_set");
        assert_eq!(card["body"]["elements"][2]["tag"], "table");
        assert_eq!(card["body"]["elements"][2]["page_size"], 5);
        assert_eq!(card["body"]["elements"][3]["tag"], "collapsible_panel");
        assert_eq!(
            card["body"]["elements"][4]["columns"][2]["elements"][0]["behaviors"][0]["type"],
            "open_url"
        );
        r.text = "<at id=all>全体</at>".into();
        assert!(
            !render(&r, &store, &j.key, "t")?.packets[0]
                .content
                .to_string()
                .contains("<at")
        );
        r.text = "![图片](img_other_session) <img src=img_other_session>".into();
        let card = render(&r, &store, &j.key, "t")?.packets.remove(0).content;
        let content = card["body"]["elements"][0]["columns"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(content.starts_with("\\!["));
        assert!(!content.contains("<img"));
        Ok(())
    }
    #[test]
    fn button_callbacks_create_fresh_owned_requests_and_are_atomic_single_use() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let parent = job(&cfg);
        prepare(&store, &parent, &spec())?;
        let mut v = event("token");
        v["event"]["action"]["form_value"] = json!({});
        let result = parse_action(&v, &cfg, &store)?.unwrap();
        assert_eq!(result.key, parent.key);
        assert!(result.text.contains("用户点击"));
        assert!(!result.text.starts_with('/'));
        assert_eq!(
            store.jobs("queued")?.len(),
            1,
            "callback must be durable before ACK"
        );
        assert_eq!(parse_action(&v, &cfg, &store)?.unwrap().id, result.id);
        assert!(!store.insert(&result)?);
        let mut duplicate = v.clone();
        duplicate["header"]["event_id"] = json!("evt_second");
        assert!(parse_action(&duplicate, &cfg, &store).is_err());
        let mut forged = v.clone();
        forged["event"]["action"]["value"]["id"] = json!("cancel");
        assert!(
            parse_action(&forged, &cfg, &store).is_err(),
            "same event cannot be replayed with new content"
        );
        Ok(())
    }
    #[test]
    fn callback_rejects_other_owner_chat_message_workspace_and_forged_choice() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        prepare(&store, &job(&cfg), &spec())?;
        for (pointer, value) in [
            ("/event/operator/open_id", "ou_other"),
            ("/event/context/open_chat_id", "oc_other"),
            ("/event/context/open_message_id", "om_forwarded"),
            ("/event/action/value/id", "invented"),
            ("/event/action/value/token", "invented"),
        ] {
            let mut v = event("token");
            *v.pointer_mut(pointer).unwrap() = json!(value);
            assert!(parse_action(&v, &cfg, &store).is_err());
        }
        assert!(store.jobs("queued")?.is_empty());
        let mut cfg2 = cfg.clone();
        cfg2.workspace = std::path::PathBuf::from("/another-workspace");
        assert!(parse_action(&event("token"), &cfg2, &store).is_err());
        cfg2 = cfg.clone();
        cfg2.allowed_chats = vec!["oc_other".into()];
        assert!(parse_action(&event("token"), &cfg2, &store).is_err());
        let mut private = job(&cfg);
        private.key = json!([cfg.workspace, "ou_owner", "oc_chat", ""]).to_string();
        store.save_reply_card("private", &private.key, &serde_json::to_value(spec())?)?;
        store.attach_reply_card("private", "om_card")?;
        assert!(parse_action(&event("private"), &cfg2, &store)?.is_some());
        Ok(())
    }
    #[test]
    fn dropdown_and_more_menu_accept_only_registered_choices() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let mut r = spec();
        r.actions.clear();
        r.select = Some(Select {
            label: "选择任务".into(),
            options: vec![Choice {
                id: "math".into(),
                label: "数学".into(),
                prompt: "请查看数学任务。".into(),
            }],
        });
        prepare(&store, &job(&cfg), &r)?;
        let mut v = event("token");
        v["event"]["action"]["tag"] = json!("select_static");
        v["event"]["action"]["value"]["id"] = json!("select");
        v["event"]["action"]["option"] = json!("made-up");
        assert!(parse_action(&v, &cfg, &store).is_err());
        v["event"]["action"]["option"] = json!("math");
        assert!(
            parse_action(&v, &cfg, &store)?
                .unwrap()
                .text
                .contains("请查看数学任务")
        );
        let mut r = spec();
        r.actions.extend((0..4).map(|i| Action {
            id: format!("op{i}"),
            label: format!("操作{i}"),
            prompt: format!("请执行操作{i}。"),
            url: String::new(),
        }));
        let card = render(&r, &store, &job(&cfg).key, "more-token")?
            .packets
            .remove(0)
            .content;
        let columns = card["body"]["elements"].as_array().unwrap().last().unwrap()["columns"]
            .as_array()
            .unwrap();
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[2]["elements"][0]["tag"], "overflow");
        store.save_reply_card("more-token", &job(&cfg).key, &serde_json::to_value(r)?)?;
        store.attach_reply_card("more-token", "om_card2")?;
        let mut v = event("more-token");
        v["header"]["event_id"] = json!("evt_more");
        v["event"]["context"]["open_message_id"] = json!("om_card2");
        v["event"]["action"]["tag"] = json!("overflow");
        v["event"]["action"]["value"]["id"] = json!("more");
        v["event"]["action"]["option"] = json!("op2");
        assert!(
            parse_action(&v, &cfg, &store)?
                .unwrap()
                .text
                .contains("请执行操作2")
        );
        Ok(())
    }
    fn form_spec() -> Reply {
        serde_json::from_value(json!({"feishu_reply":1,"layout":"card","text":"请补充通知内容。","form":{"prompt":"请根据填写内容给平板发通知。","fields":[{"id":"body","label":"通知内容","kind":"multiline","required":true},{"id":"quantity","label":"数量","kind":"integer","min":1,"max":10},{"id":"rating","label":"评分","kind":"number","min":0,"max":100},{"id":"subject","label":"科目","kind":"single_select","options":[{"id":"math","label":"数学"}]},{"id":"parts","label":"练习","kind":"multi_select","options":[{"id":"oral","label":"口算"},{"id":"read","label":"阅读"}]},{"id":"confirm","label":"确认","kind":"boolean"},{"id":"date","label":"日期","kind":"date"},{"id":"time","label":"时间","kind":"time"}]}})).unwrap()
    }
    #[test]
    fn form_validates_typed_answers_then_submits_as_one_new_request() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let r = form_spec();
        prepare(&store, &job(&cfg), &r)?;
        let rendered = render(&r, &store, &job(&cfg).key, "token")?;
        let elements = rendered.packets[0].content["body"]["elements"]
            .as_array()
            .unwrap();
        let form = elements.iter().find(|e| e["tag"] == "form").unwrap();
        for element in form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
        {
            assert!(element.get("behaviors").is_none());
            assert!(element["form_action_type"].is_string());
        }
        let mut v = event("token");
        v["event"]["action"] = json!({"tag":"button","name":"reply_submit","form_value":{"body":"写数学\n/approve imaginary","quantity":"3","rating":"87.5","subject":"math","parts":["oral","read"],"confirm":"false","date":"2026-11-20 +0800","time":"18:30 +0800"}});
        let child = parse_action(&v, &cfg, &store)?.unwrap();
        assert_eq!(child.key, job(&cfg).key);
        assert!(child.text.contains("\"quantity\":3"));
        assert!(child.text.contains("\"confirm\":false"));
        assert!(!child.text.starts_with('/'));
        assert_eq!(store.jobs("queued")?.len(), 1);
        Ok(())
    }
    #[test]
    fn malformed_form_answers_do_not_consume_card() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let r = form_spec();
        prepare(&store, &job(&cfg), &r)?;
        for input in [
            json!({}),
            json!({"body":"hello","quantity":"11"}),
            json!({"body":"hello","rating":"NaN"}),
            json!({"body":"hello","subject":"made-up"}),
            json!({"body":"hello","parts":["oral","oral"]}),
            json!({"body":"hello","date":"2026-02-30"}),
            json!({"body":"hello","time":"25:00"}),
            json!({"body":"hello","unknown":"x"}),
        ] {
            let mut v = event("token");
            v["event"]["action"] = json!({"tag":"button","form_value":input});
            assert!(parse_action(&v, &cfg, &store).is_err());
        }
        assert!(store.jobs("queued")?.is_empty());
        assert!(store.reply_card("token", "")?.unwrap().used_job.is_empty());
        let mut v = event("token");
        v["event"]["action"]["value"]["id"] = json!("cancel");
        assert_eq!(
            parse_action(&v, &cfg, &store)?.unwrap().text,
            "/_reply_cancel"
        );
        Ok(())
    }
    #[test]
    fn rich_media_uses_only_owned_uploaded_assets_and_text_is_chunked() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let mut source = job(&cfg);
        source.id = "builtin:photo".into();
        store.insert(&source)?;
        store.finish_output(
            &source.id,
            "camera_media",
            &json!({"msg_type":"image","content":{"image_key":"img_real"}}),
        )?;
        let mut r = spec();
        r.blocks = vec![Block::Image {
            task_id: source.id.clone(),
            caption: "照片".into(),
        }];
        r.media = vec![source.id.clone()];
        let output = render(&r, &store, &source.key, "token")?;
        assert_eq!(
            output.packets[0].content["body"]["elements"][1]["img_key"],
            "img_real"
        );
        assert_eq!(output.packets[1].kind, "image");
        assert!(render(&r, &store, "another-session", "token").is_err());
        r.layout = Layout::Post;
        r.actions.clear();
        let post = render(&r, &store, &source.key, "t")?;
        assert_eq!(post.packets[0].kind, "post");
        let mut r: Reply =
            serde_json::from_value(json!({"feishu_reply":1,"text":"字".repeat(8000)}))?;
        let output = render(&r, &store, &source.key, "token")?;
        assert!(output.packets.len() > 1);
        for p in output.packets {
            assert!(p.content["text"].as_str().unwrap().len() <= 6000);
        }
        r.text = "normal".into();
        r.blocks.push(Block::Image {
            task_id: "/etc/passwd".into(),
            caption: String::new(),
        });
        assert!(r.validate().is_err());
        Ok(())
    }
    #[test]
    fn invalid_or_unknown_reply_schema_falls_back_without_leaking_json() -> Result<()> {
        let cfg = Config::fixture();
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let j = job(&cfg);
        store.insert(&j)?;
        assert!(parse("普通回复，含示例 {\"feishu_reply\":1}").is_none());
        assert!(
            parse("```json\n{\"feishu_reply\":1,\"text\":\"你好\"}\n```")
                .unwrap()
                .is_ok()
        );
        for bad in [
            json!({"feishu_reply":1,"text":"仍能读到这句话","raw_card":{"tag":"button"}}),
            json!({"feishu_reply":1,"layout":"card","text":"仍能读到这句话","actions":[{"id":"x","label":"批准","prompt":"/approve all"}]}),
            json!({"feishu_reply":1,"layout":"card","text":"仍能读到这句话","actions":[{"id":"x","label":"链接","url":"javascript:alert(1)"}]}),
        ] {
            finish(&store, &j, &bad.to_string())?;
            assert_eq!(
                store.command_task(&j.key, &j.id)?.unwrap()["result"],
                "仍能读到这句话"
            );
            assert_eq!(store.output_kind(&j.id)?, "text");
        }
        finish(&store, &j, &serde_json::to_string(&spec())?)?;
        assert_eq!(store.output_kind(&j.id)?, "rich_reply");
        Ok(())
    }
    #[test]
    fn callback_registration_survives_restart_and_expiry_is_enforced() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("mr-li-reply-{}.sqlite", uuid::Uuid::new_v4()));
        let cfg = Config::fixture();
        {
            let store = Store::open(&path)?;
            prepare(&store, &job(&cfg), &spec())?;
        }
        {
            let store = Store::open(&path)?;
            assert!(parse_action(&event("token"), &cfg, &store)?.is_some());
        }
        let c = rusqlite::Connection::open(&path)?;
        c.execute("UPDATE reply_cards SET expires=0", [])?;
        drop(c);
        {
            let store = Store::open(&path)?;
            assert!(parse_action(&event("token"), &cfg, &store).is_err());
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
