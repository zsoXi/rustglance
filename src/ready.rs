//! Opt-in task readiness. Does not relax write policy or claim global network idle.
use crate::{Class, Ctx, Refusal};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ready {
    text: Option<String>,
    url: Option<String>,
    selector: Option<String>,
    #[serde(default = "one")]
    min_count: usize,
    #[serde(default)]
    min_text_length: usize,
    #[serde(default = "timeout")]
    timeout_ms: u64,
}
fn one() -> usize {
    1
}
fn timeout() -> u64 {
    8000
}

pub(crate) fn parse(v: Option<&Value>) -> Result<Option<Ready>, Refusal> {
    let Some(v) = v else { return Ok(None) };
    let r: Ready = serde_json::from_value(v.clone()).map_err(|e| Refusal::bad_args(format!("wait_for: {e}")))?;
    if r.text.as_deref().is_some_and(|s| s.trim().is_empty())
        || r.url.as_deref().is_some_and(|s| s.trim().is_empty())
        || r.selector.as_deref().is_some_and(|s| s.trim().is_empty())
        || (r.text.is_none() && r.selector.is_none())
        || r.min_count == 0
        || r.min_count > 10000
        || r.timeout_ms == 0
        || r.timeout_ms > 120000
        || (r.selector.is_none() && (r.min_count != 1 || r.min_text_length != 0))
    {
        return Err(Refusal::bad_args(
            "wait_for needs nonempty text or selector; url alone is insufficient; count 1..10000, timeout 1..120000; count/text length require selector",
        ));
    }
    Ok(Some(r))
}

pub fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"anyOf":[{"required":["text"]},{"required":["selector"]}],"properties":{
        "text":{"type":"string","minLength":1},"url":{"type":"string","minLength":1},
        "selector":{"type":"string","minLength":1,"description":"CSS selector of useful content, not executable code"},
        "min_count":{"type":"integer","minimum":1,"maximum":10000,"default":1},
        "min_text_length":{"type":"integer","minimum":0,"description":"Minimum combined visible text length of matched nodes"},
        "timeout_ms":{"type":"integer","minimum":1,"maximum":120000,"default":8000}
    }})
}

const PROBE: &str = r#"(a) => {
 const visible=e=>{
   if(!e || !e.getClientRects().length || getComputedStyle(e).visibility==='hidden')return false;
   for(let p=e;p;p=p.parentElement){const s=getComputedStyle(p);if(s.display==='none'||Number(s.opacity)===0)return false;}
   return true;
 };
 let nodes=[];
 try { if(a.selector) nodes=[...document.querySelectorAll(a.selector)].filter(visible); }
 catch(e){return {bad_selector:true};}
 const body=a.text==null?'':(()=>{const out=[];const w=document.createTreeWalker(document.body||document,NodeFilter.SHOW_TEXT);let n;while((n=w.nextNode())){const p=n.parentElement;if(p&&!p.closest('script,style,noscript')&&visible(p))out.push(n.textContent);}return out.join(' ');})();
 const text=nodes.map(e=>e.innerText||e.textContent||'').join('\n');
 const conditions={document:document.readyState!=='loading',fresh:a.previous==null||performance.timeOrigin!==a.previous};
 if(a.url!=null) conditions.url=location.href.includes(a.url);
 if(a.text!=null) conditions.text=body.includes(a.text);
 if(a.selector!=null){conditions.count=nodes.length>=a.min_count;conditions.length=text.length>=a.min_text_length;}
 return {pass:Object.values(conditions).every(Boolean),conditions,count:nodes.length,text_length:text.length,
 signature:JSON.stringify([performance.timeOrigin,location.href,a.selector?text:a.text,nodes.length])};
}"#;

pub(crate) async fn validate_selector(ctx: &Ctx, r: Option<&Ready>) -> Result<(), Refusal> {
    if let Some(selector) = r.and_then(|r| r.selector.as_ref()) {
        let valid = ctx
            .eval(
                "(s) => {try{document.querySelector(s);return true;}catch{return false;}}",
                &json!(selector),
            )
            .await?;
        if valid != true {
            return Err(Refusal::bad_args(
                "wait_for selector is invalid CSS; action was not performed",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn wait(ctx: &Ctx, r: &Ready, previous: Option<f64>) -> Result<Value, Refusal> {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(r.timeout_ms);
    let mut stable: Option<(String, Instant)> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Refusal::new(
                Class::Timeout,
                "wait_for deadline expired",
                "inspect tree/logs before repeating the action",
            ));
        }
        let probe = ctx.eval_with_timeout(PROBE, &json!({"text":r.text,"url":r.url,"selector":r.selector,"min_count":r.min_count,"min_text_length":r.min_text_length,"previous":previous}), remaining).await;
        match probe {
            Ok(v) => {
                if v["bad_selector"] == true {
                    return Err(Refusal::bad_args("wait_for selector is invalid CSS"));
                }
                if v["pass"] == true && Instant::now() <= deadline {
                    let signature = v["signature"].as_str().unwrap_or("");
                    match &stable {
                        Some((s, t)) if s == signature && t.elapsed() >= Duration::from_millis(200) => {
                            return Ok(
                                json!({"mode":"condition","ready":true,"network_idle_checked":false,"stable_ms":200,"waited_ms":start.elapsed().as_millis(),"conditions":v["conditions"],"count":v["count"],"text_length":v["text_length"]}),
                            );
                        }
                        Some((s, _)) if s == signature => {}
                        _ => stable = Some((signature.to_owned(), Instant::now())),
                    }
                } else {
                    stable = None;
                }
                if start.elapsed() >= Duration::from_millis(r.timeout_ms) {
                    return Err(Refusal::new(
                        Class::Timeout,
                        format!("wait_for not satisfied stably: {}", v["conditions"]),
                        "inspect tree/logs; do not repeat a state-changing action without checking its effect",
                    ));
                }
            }
            Err(e)
                if matches!(e.class, Class::PageError | Class::Browser)
                    && start.elapsed() < Duration::from_millis(r.timeout_ms) =>
            {
                stable = None;
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_contract() {
        assert!(parse(None).unwrap().is_none());
        for v in [
            json!({}),
            json!({"url":"x"}),
            json!({"text":""}),
            json!({"text":"   "}),
            json!({"selector":"  "}),
            json!({"text":"ok","timeout_ms":120001}),
            json!({"selector":"h3","min_count":10001}),
            json!({"text":"ok","timeout_ms":0}),
            json!({"selector":"h3","min_count":0}),
            json!({"text":"ok","min_count":2}),
            json!({"selector":"h3","unknown":1}),
        ] {
            assert!(parse(Some(&v)).is_err(), "{v}");
        }
        assert!(
            parse(Some(
                &json!({"selector":"main h3","min_count":3,"min_text_length":30,"url":"/search"})
            ))
            .unwrap()
            .is_some()
        );
        assert!(parse(Some(&json!({"text":"Ready"}))).unwrap().is_some());
    }
}
