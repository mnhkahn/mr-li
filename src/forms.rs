//! Typed OpenCode forms rendered one field at a time, so `when` conditions use real answers.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub fn status(title: &str, text: &str) -> Value {
    json!({"schema":"2.0","config":{"update_multi":true,"enable_forward":false},
        "header":{"title":{"tag":"plain_text","content":title},"template":"blue"},
        "body":{"elements":[{"tag":"markdown","content":text}]}})
}
pub fn visible(field: &Value, answers: &Value) -> bool {
    field["hidden"] != true
        && field["when"].as_array().is_none_or(|conditions| {
            conditions.iter().all(|c| {
                let v = &answers[c["key"].as_str().unwrap_or("")];
                match c["op"].as_str() {
                    Some("eq") => v == &c["value"],
                    Some("neq") => v != &c["value"],
                    _ => false,
                }
            })
        })
}
pub fn next(form: &Value, answers: &Value, start: usize) -> Option<usize> {
    form["fields"]
        .as_array()?
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, f)| visible(f, answers))
        .map(|(i, _)| i)
}
fn button(label: &str, token: &str, revision: u64, operation: &str, submit: bool) -> Value {
    let mut b = json!({"tag":"button","text":{"tag":"plain_text","content":label},
        "type":if submit {"primary"} else {"default"},
        "behaviors":[{"type":"callback","value":{"action":"opencode_form","token":token,"revision":revision,"operation":operation}}]});
    if submit {
        b["form_action_type"] = json!("submit");
        b["name"] = json!("next");
    }
    b
}
pub fn card(state: &Value, token: &str, error: Option<&str>) -> Result<Value> {
    let form = &state["form"];
    let index = state["index"].as_u64().context("表单进度缺失")? as usize;
    let rev = state["revision"].as_u64().unwrap_or(0);
    let mut card = status(form["title"].as_str().unwrap_or("需要补充信息"), "");
    let mut elements = vec![];
    if let Some(error) = error {
        elements.push(json!({"tag":"markdown","content":format!("❗ {error}")}));
    }
    if let Some(f) = form["fields"].get(index) {
        let title = f["title"]
            .as_str()
            .or(f["description"].as_str())
            .or(f["key"].as_str())
            .unwrap_or("请填写");
        elements.push(json!({"tag":"markdown","content":format!("**{}{}**",title,if f["required"]==true {"（必填）"} else {"（可跳过）"})}));
        if f["title"].is_string()
            && let Some(d) = f["description"].as_str()
        {
            elements.push(json!({"tag":"markdown","content":d}));
        }
        let kind = f["type"].as_str().unwrap_or("");
        if kind == "external" {
            let url = f["url"].as_str().context("外部表单链接缺失")?;
            ensure!(
                url.starts_with("https://") || url.starts_with("http://"),
                "外部链接必须使用 HTTP(S)"
            );
            elements.push(json!({"tag":"button","text":{"tag":"plain_text","content":"打开链接完成操作"},"type":"primary","behaviors":[{"type":"open_url","default_url":url}]}));
            elements.push(button("已完成，继续", token, rev, "external_next", false));
        } else {
            let options = if kind == "boolean" {
                json!([{"label":"是","value":"true"},{"label":"否","value":"false"}])
            } else {
                f["options"].clone()
            };
            let has_options = options.as_array().is_some_and(|o| !o.is_empty());
            let mut controls = vec![];
            if has_options {
                let mut control = json!({"tag":if kind=="multiselect" {"multi_select_static"} else {"select_static"},"name":"answer","width":"fill",
                    "placeholder":{"tag":"plain_text","content":"请选择"},"options":options.as_array().unwrap().iter().map(|o|json!({"text":{"tag":"plain_text","content":o["label"]},"value":o["value"]})).collect::<Vec<_>>()});
                if !f["default"].is_null() {
                    if kind == "multiselect" {
                        control["selected_values"] = f["default"].clone();
                    } else {
                        control["initial_option"] = json!(if kind == "boolean" {
                            f["default"].to_string()
                        } else {
                            f["default"].as_str().unwrap_or("").to_string()
                        });
                    }
                }
                controls.push(control);
                for o in options.as_array().unwrap() {
                    if let Some(d) = o["description"].as_str() {
                        elements.push(json!({"tag":"markdown","content":format!("{}：{d}",o["label"].as_str().unwrap_or(""))}));
                    }
                }
            }
            if !has_options || f["custom"] == true {
                let hint = if has_options {
                    "自定义答案（填写后优先使用；多选每行一项）"
                } else if kind == "multiselect" {
                    "每行填写一项"
                } else {
                    f["placeholder"].as_str().unwrap_or("请输入答案")
                };
                let mut input = json!({"tag":"input","name":if has_options {"custom"} else {"answer"},"input_type":"multiline_text","placeholder":{"tag":"plain_text","content":hint},"width":"fill"});
                if !has_options && !f["default"].is_null() {
                    input["default_value"] = json!(
                        f["default"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| f["default"].to_string())
                    );
                }
                controls.push(input);
            }
            controls.push(button("确认并继续", token, rev, "next", true));
            elements.push(json!({"tag":"form","name":"answers","elements":controls}));
            if f["required"] != true {
                elements.push(button("跳过此项", token, rev, "skip", false));
            }
        }
        elements.push(json!({"tag":"markdown","content":format!("第 {} / {} 项。逐项确认后统一提交。",index+1,form["fields"].as_array().map_or(0,Vec::len))}));
    } else {
        elements.push(json!({"tag":"markdown","content":"请核对答案，点击提交后继续任务。"}));
        if let Some(fields) = form["fields"].as_array() {
            for f in fields {
                let key = f["key"].as_str().unwrap_or("");
                if let Some(value) = state["answers"].get(key) {
                    let display = |v: &Value| {
                        f["options"]
                            .as_array()
                            .and_then(|opts| opts.iter().find(|o| o["value"] == *v))
                            .and_then(|o| o["label"].as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
                            })
                    };
                    let text = value
                        .as_array()
                        .map(|vs| vs.iter().map(display).collect::<Vec<_>>().join("、"))
                        .unwrap_or_else(|| display(value));
                    elements.push(json!({"tag":"markdown","content":format!("**{}**：{}",f["title"].as_str().unwrap_or(key),text)}));
                }
            }
        }
        elements.push(button("提交答案", token, rev, "submit", false));
    }
    elements.push(button("重新填写", token, rev, "restart", false));
    elements.push(button("取消本次提问", token, rev, "cancel", false));
    card["body"]["elements"] = json!(elements);
    Ok(card)
}
pub fn answer(field: &Value, input: &Value) -> Result<Value> {
    let custom = input["custom"].as_str().filter(|s| !s.trim().is_empty());
    ensure!(
        custom.is_none() || field["custom"] == true,
        "此字段不允许自定义答案"
    );
    let raw = custom
        .map(|s| json!(s))
        .unwrap_or_else(|| input["answer"].clone());
    let text = raw.as_str().unwrap_or("");
    let value = match field["type"].as_str() {
        Some("string") => {
            ensure!(raw.is_string(), "请输入或选择答案");
            raw
        }
        Some("boolean") => json!(text.parse::<bool>().context("请选择是或否")?),
        Some("integer") => json!(text.trim().parse::<i64>().context("请输入整数")?),
        Some("number") => {
            let n = text.trim().parse::<f64>().context("请输入数字")?;
            ensure!(n.is_finite(), "请输入有限数字");
            json!(n)
        }
        Some("multiselect") => {
            if raw.is_array() {
                let _: Vec<String> =
                    serde_json::from_value(raw.clone()).context("请选择有效选项")?;
                raw
            } else {
                json!(
                    text.lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                )
            }
        }
        _ => anyhow::bail!("不支持的表单字段，请升级桥接程序"),
    };
    let empty =
        value.as_str().is_some_and(str::is_empty) || value.as_array().is_some_and(Vec::is_empty);
    ensure!(field["required"] != true || !empty, "此项必填");
    if let Some(opts) = field["options"].as_array()
        && field["custom"] != true
        && !empty
    {
        let values = value
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]);
        ensure!(
            values.iter().all(|v| opts.iter().any(|o| o["value"] == *v)),
            "请选择列表中的选项"
        );
    }
    let len = value
        .as_str()
        .map(|s| s.chars().count())
        .or_else(|| value.as_array().map(Vec::len));
    if let Some(len) = len {
        let (min, max) = if value.is_array() {
            ("minItems", "maxItems")
        } else {
            ("minLength", "maxLength")
        };
        ensure!(
            field[min].as_u64().is_none_or(|n| len as u64 >= n),
            "答案长度或选项数量不足"
        );
        ensure!(
            field[max].as_u64().is_none_or(|n| len as u64 <= n),
            "答案长度或选项数量超限"
        );
    }
    if let Some(n) = value.as_f64() {
        ensure!(
            field["minimum"].as_f64().is_none_or(|min| n >= min),
            "数值小于最小值"
        );
        ensure!(
            field["maximum"].as_f64().is_none_or(|max| n <= max),
            "数值大于最大值"
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_search_choices_keep_machine_values_and_submit_callback() -> Result<()> {
        let form = json!({"title":"搜索设置","fields":[{"key":"choice","type":"string","required":true,"custom":false,"description":"Allow OpenCode to search the web?","options":[{"label":"Allow search","value":"allow"},{"label":"Disable web search","value":"disable"}]}]});
        let c = card(&json!({"form":form,"index":0,"revision":3}), "token", None)?;
        let controls = &c["body"]["elements"][1]["elements"];
        assert_eq!(controls[0]["tag"], "select_static");
        assert_eq!(controls[0]["options"][0]["value"], "allow");
        assert_eq!(controls[1]["form_action_type"], "submit");
        assert_eq!(controls[1]["behaviors"][0]["value"]["revision"], 3);
        assert_eq!(
            answer(&form["fields"][0], &json!({"answer":"allow"}))?,
            json!("allow")
        );
        assert!(answer(&form["fields"][0], &json!({"answer":"Allow search"})).is_err());
        Ok(())
    }
    #[test]
    fn typed_answers_validate_required_bounds_and_custom_multiselect() -> Result<()> {
        assert_eq!(
            answer(
                &json!({"type":"boolean","required":true}),
                &json!({"answer":"false"})
            )?,
            false
        );
        assert_eq!(
            answer(
                &json!({"type":"integer","minimum":0}),
                &json!({"answer":"2"})
            )?,
            2
        );
        assert!(answer(&json!({"type":"integer"}), &json!({"answer":"2.5"})).is_err());
        assert!(
            answer(
                &json!({"type":"number","maximum":1}),
                &json!({"answer":"2.5"})
            )
            .is_err()
        );
        assert!(answer(&json!({"type":"number"}), &json!({"answer":"NaN"})).is_err());
        assert!(
            answer(
                &json!({"type":"string","required":true}),
                &json!({"answer":""})
            )
            .is_err()
        );
        assert_eq!(
            answer(
                &json!({"type":"multiselect","custom":true}),
                &json!({"answer":["a"],"custom":"x\ny"})
            )?,
            json!(["x", "y"])
        );
        assert!(
            answer(
                &json!({"type":"multiselect","maxItems":1}),
                &json!({"answer":["a","b"]})
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn hidden_and_conditional_fields_and_external_links() -> Result<()> {
        let form = json!({"fields":[{"hidden":true},{"when":[{"key":"choice","op":"eq","value":"choose"}],"type":"string"},{"type":"external","url":"https://example.com","title":"登录"}]});
        assert_eq!(next(&form, &json!({"choice":"allow"}), 0), Some(2));
        assert_eq!(next(&form, &json!({"choice":"choose"}), 0), Some(1));
        let c = card(&json!({"form":form,"index":2}), "token", None)?;
        assert_eq!(
            c["body"]["elements"][1]["behaviors"][0]["default_url"],
            "https://example.com"
        );
        let mut bad = form;
        bad["fields"][2]["url"] = json!("javascript:bad");
        assert!(card(&json!({"form":bad,"index":2}), "token", None).is_err());
        Ok(())
    }
}
