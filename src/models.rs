use crate::agent::Kind;
use anyhow::{Context, Result};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct FreeModel {
    pub id: String,
    pub name: String,
}
pub fn free_models(catalog: &Value) -> Vec<FreeModel> {
    let Some(models) = catalog["all"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["id"] == "opencode"))
        .and_then(|p| p["models"].as_object())
    else {
        return vec![];
    };
    let mut result = models
        .iter()
        .filter(|(_, m)| {
            m["cost"]["input"].as_f64() == Some(0.0)
                && m["cost"]["output"].as_f64() == Some(0.0)
                && m["status"] != "deprecated"
        })
        .map(|(id, m)| FreeModel {
            id: format!("opencode/{id}"),
            name: m["name"].as_str().unwrap_or(id).into(),
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}
pub fn scope(key: &str) -> Result<String> {
    let values: Vec<Value> = serde_json::from_str(key).context("会话标识无效")?;
    anyhow::ensure!(values.len() == 4, "会话标识无效");
    Ok(serde_json::to_string(&values[..3])?)
}
// Keep existing OpenCode preferences; other backends get independent slots.
pub fn model_scope(scope: &str, agent: Kind) -> String {
    if agent == Kind::OpenCode {
        scope.into()
    } else {
        json!([scope, agent.id()]).to_string()
    }
}
pub fn default_model(agent: Kind) -> &'static str {
    match agent {
        Kind::OpenCode => "auto",
        Kind::Antigravity => "gemini-3.8-flash-high",
        Kind::Kilo => "kilo/kilo-auto/free",
        Kind::Trae | Kind::Cline => "auto",
    }
}
pub fn choice_id(agent: Kind, model: &str) -> String {
    format!("{}:{model}", agent.id())
}
pub fn parse_choice(value: &str) -> Result<(Kind, &str)> {
    let (agent, model) = value.split_once(':').context("请选择 Agent 和模型")?;
    let agent = Kind::parse(agent)?;
    anyhow::ensure!(
        !model.is_empty()
            && value.len() <= 256
            && model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
        "模型选项无效"
    );
    Ok((agent, model))
}
pub fn runtime_models(open: Option<&Value>, cline: Option<&Value>) -> Vec<FreeModel> {
    let mut choices = Vec::new();
    for agent in Kind::ALL {
        let models = match agent {
            Kind::OpenCode => open.map(free_models).unwrap_or_default(),
            Kind::Cline => cline
                .and_then(|c| c["free"].as_array())
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    Some(FreeModel {
                        id: m["id"].as_str()?.into(),
                        name: m["name"].as_str().unwrap_or(m["id"].as_str()?).into(),
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        {
            let model = default_model(agent);
            choices.push(FreeModel {
                id: choice_id(agent, model),
                name: format!(
                    "{} · {}",
                    agent.name(),
                    match agent {
                        Kind::Kilo => "Auto Free",
                        Kind::Antigravity => "Gemini 3.8 Flash",
                        Kind::Trae => "免费账户默认模型",
                        Kind::OpenCode => "默认免费模型",
                        _ => "自动选择免费模型",
                    }
                ),
            });
        }
        choices.extend(models.into_iter().map(|m| FreeModel {
            id: choice_id(agent, &m.id),
            name: format!("{} · {}", agent.name(), m.name),
        }));
    }
    choices
}
pub fn runtime_picker(models: &[FreeModel], current: &str, token: &str, switched: bool) -> Value {
    let mut card = picker(models, current, token, switched);
    card["header"]["title"]["content"] = json!("选择 Agent 和模型");
    card["body"]["elements"][1]["behaviors"][0]["value"]["action"] = json!("select_runtime");
    card["body"]["elements"][2]["content"] = json!(
        "点选一项，同时切换 Agent 和模型。各 Agent 的模型与会话分别保留；有任务执行时请先 /stop。模型列表暂不可用时仅显示默认入口。实际可用性以免费目录和账号为准，卡片 30 分钟内有效。"
    );
    if !switched {
        let label = models
            .iter()
            .find(|m| m.id == current)
            .map(|m| m.name.as_str())
            .unwrap_or(current);
        card["body"]["elements"][0]["content"] = json!(format!("**当前选择：{label}**"));
    }
    card
}
pub fn picker(models: &[FreeModel], current: &str, token: &str, switched: bool) -> Value {
    let name = models
        .iter()
        .find(|m| m.id == current)
        .map(|m| m.name.as_str())
        .unwrap_or(current);
    let mut select = json!({"tag":"select_static","name":"model_selector","width":"fill",
        "placeholder":{"tag":"plain_text","content":"展开列表，点选即切换"},
        "options":models.iter().map(|m|json!({"text":{"tag":"plain_text","content":m.name},"value":m.id})).collect::<Vec<_>>(),
        "behaviors":[{"type":"callback","value":{"action":"select_model","token":token}}]});
    if models.iter().any(|m| m.id == current) {
        select["initial_option"] = json!(current);
    }
    json!({"schema":"2.0","config":{"update_multi":true,"width_mode":"default","enable_forward":false,
    "summary":{"content":format!("李老师 · 模型选择 · {name}")}},
    "header":{"title":{"tag":"plain_text","content":"选择模型"},"template":if switched{"green"}else{"blue"}},
    "body":{"direction":"vertical","vertical_spacing":"12px","padding":"12px","elements":[
        {"tag":"markdown","content":format!("**{}：{}**",if switched{"已切换"}else{"当前模型"},name)},
        select,
        {"tag":"markdown","text_size":"notation","content":"仅展示 OpenCode 免费模型。选择只影响你在此聊天的后续任务；正在执行的任务不变。卡片 30 分钟内有效。"}
    ]}})
}
pub fn refresh_selection(card: &Value, current: &str) -> Result<Value> {
    let selector = &card["body"]["elements"][1];
    let token = selector["behaviors"][0]["value"]["token"]
        .as_str()
        .context("卡片标识缺失")?;
    let models = selector["options"]
        .as_array()
        .context("卡片模型列表缺失")?
        .iter()
        .map(|option| {
            Ok(FreeModel {
                id: option["value"].as_str().context("模型选项缺失")?.into(),
                name: option["text"]["content"]
                    .as_str()
                    .context("模型名称缺失")?
                    .into(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let build = if selector["behaviors"][0]["value"]["action"] == "select_runtime" {
        runtime_picker
    } else {
        picker
    };
    Ok(build(
        &models,
        current,
        token,
        card["header"]["template"] == "green",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unified_picker_lists_free_pairs_and_survives_catalog_outages() -> Result<()> {
        let open = json!({"all":[{"id":"opencode","models":{
            "free":{"name":"Free","cost":{"input":0,"output":0}},
            "paid":{"cost":{"input":1,"output":1}}
        }}]});
        let cline =
            json!({"free":[{"id":"cline-free/test","name":"Promo"}],"recommended":[{"id":"paid"}]});
        let options = runtime_models(Some(&open), Some(&cline));
        assert!(options.iter().any(|o| o.id == "opencode:opencode/free"));
        assert!(options.iter().any(|o| o.id == "cline:cline-free/test"));
        assert!(!options.iter().any(|o| o.id.contains("paid")));
        for option in &options {
            parse_choice(&option.id)?;
        }
        assert_eq!(runtime_models(None, None).len(), 5);
        let old = runtime_picker(&options, "opencode:opencode/free", "token", true);
        let refreshed = refresh_selection(&old, "kilo:kilo/kilo-auto/free")?;
        assert_eq!(
            refreshed["body"]["elements"][1]["initial_option"],
            "kilo:kilo/kilo-auto/free"
        );
        assert_eq!(
            refreshed["body"]["elements"][1]["behaviors"][0]["value"]["action"],
            "select_runtime"
        );
        for bad in [
            "kilo",
            "cline:",
            "unknown:auto",
            "kilo:auto paid",
            "cline:a:bad",
        ] {
            assert!(parse_choice(bad).is_err());
        }
        Ok(())
    }
    #[test]
    fn picker_only_contains_verified_free_models() {
        let catalog = json!({"all":[{"id":"opencode","models":{
            "free":{"name":"Free","cost":{"input":0,"output":0}},
            "paid":{"cost":{"input":0,"output":2}},"unknown":{},
            "old":{"status":"deprecated","cost":{"input":0,"output":0}}}},
            {"id":"other","models":{"free":{"cost":{"input":0,"output":0}}}}]});
        let models = free_models(&catalog);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "opencode/free");
        let card = picker(&models, "opencode/free", "nonce", false);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(
            card["body"]["elements"][1]["initial_option"],
            "opencode/free"
        );
    }
    #[test]
    fn delayed_card_delivery_displays_latest_selection() -> Result<()> {
        let models = vec![
            FreeModel {
                id: "opencode/a".into(),
                name: "A".into(),
            },
            FreeModel {
                id: "opencode/b".into(),
                name: "B".into(),
            },
        ];
        let old = picker(&models, "opencode/a", "nonce", true);
        let current = refresh_selection(&old, "opencode/b")?;
        assert_eq!(
            current["body"]["elements"][1]["initial_option"],
            "opencode/b"
        );
        assert_eq!(current["body"]["elements"][0]["content"], "**已切换：B**");
        assert_eq!(
            current["body"]["elements"][1]["behaviors"][0]["value"]["token"],
            "nonce"
        );
        Ok(())
    }
    #[test]
    fn scope_is_per_user_chat_and_survives_new_threads() {
        assert_eq!(
            scope(r#"["workspace","user","chat","root1"]"#).unwrap(),
            scope(r#"["workspace","user","chat","root2"]"#).unwrap()
        );
        assert_ne!(
            scope(r#"["workspace","user","chat","root1"]"#).unwrap(),
            scope(r#"["workspace","other","chat","root1"]"#).unwrap()
        );
    }
}
