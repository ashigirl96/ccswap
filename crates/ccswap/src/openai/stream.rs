//! Incremental Responses SSE to Anthropic Messages SSE.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};

use serde_json::{Value, json};

use super::Failure;

pub(super) fn event(output: &mut dyn Write, value: &Value) -> std::io::Result<()> {
    writeln!(
        output,
        "event: {}\ndata: {value}\n",
        value["type"].as_str().unwrap_or("error")
    )?;
    output.flush()
}

pub(super) fn translate(
    reader: impl BufRead,
    output: &mut dyn Write,
    model: &str,
    streaming: bool,
) -> Result<Value, Failure> {
    let mut translation = Translation {
        output,
        streaming,
        started: false,
        message: json!({"id":"msg_ccswap","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}),
        blocks: BTreeMap::new(),
        calls: BTreeMap::new(),
        open: BTreeSet::new(),
    };
    let mut reader = reader;
    loop {
        let data = read_event(&mut reader)?
            .ok_or_else(|| Failure::upstream("OpenAI stream ended before response.completed"))?;
        let value: Value = serde_json::from_str(&data)
            .map_err(|_| Failure::upstream("OpenAI returned an invalid stream event"))?;
        if value["type"] == "response.created"
            && let Some(id) = value["response"]["id"].as_str()
        {
            translation.message["id"] = id.into();
        }
        translation.start()?;
        match value["type"].as_str() {
            Some("response.content_part.added") if value["part"]["type"] == "output_text" => {
                translation.text_block(&value)?;
            }
            Some("response.output_text.delta") => {
                let index = translation.text_block(&value)?;
                let delta = value["delta"]
                    .as_str()
                    .ok_or_else(|| Failure::upstream("OpenAI returned an invalid text delta"))?;
                if !translation.streaming {
                    let text = translation.message["content"][index]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                        + delta;
                    translation.message["content"][index]["text"] = text.into();
                }
                translation.send(&json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":delta}}))?;
            }
            Some("response.content_part.done") if value["part"]["type"] == "output_text" => {
                if let Some(index) = translation.blocks.get(&key(&value)?).copied() {
                    translation.close(index)?;
                }
            }
            Some("response.output_item.added") if value["item"]["type"] == "function_call" => {
                translation.tool_block(&value)?;
            }
            Some("response.function_call_arguments.delta") => {
                let index = translation.tool_index(&value)?;
                let delta = value["delta"].as_str().ok_or_else(|| {
                    Failure::upstream("OpenAI returned invalid function arguments")
                })?;
                translation.send(&json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":delta}}))?;
            }
            Some("response.function_call_arguments.done") => {
                let index = translation.tool_index(&value)?;
                translation.tool_input(index, &value["arguments"])?;
                translation.close(index)?;
            }
            Some("response.output_item.done") if value["item"]["type"] == "function_call" => {
                let index = translation.tool_index(&value)?;
                translation.tool_input(index, &value["item"]["arguments"])?;
                translation.close(index)?;
            }
            Some("response.completed") => {
                for index in translation.open.clone() {
                    translation.close(index)?;
                }
                let usage = &value["response"]["usage"];
                translation.message["usage"] = json!({"input_tokens":usage["input_tokens"].as_u64().unwrap_or(0),"output_tokens":usage["output_tokens"].as_u64().unwrap_or(0)});
                let reason = if translation.calls.is_empty() {
                    "end_turn"
                } else {
                    "tool_use"
                };
                translation.message["stop_reason"] = reason.into();
                translation.send(&json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":translation.message["usage"]}))?;
                translation.send(&json!({"type":"message_stop"}))?;
                return Ok(translation.message);
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                return Err(Failure::upstream("OpenAI did not complete the response"));
            }
            _ => {}
        }
    }
}

struct Translation<'a> {
    output: &'a mut dyn Write,
    streaming: bool,
    started: bool,
    message: Value,
    blocks: BTreeMap<(u64, u64), usize>,
    calls: BTreeMap<u64, (usize, String)>,
    open: BTreeSet<usize>,
}

impl Translation<'_> {
    fn send(&mut self, value: &Value) -> Result<(), Failure> {
        if self.streaming {
            event(self.output, value).map_err(|_| Failure::upstream("Client disconnected"))?;
        }
        Ok(())
    }
    fn start(&mut self) -> Result<(), Failure> {
        if !self.started {
            self.send(&json!({"type":"message_start","message":self.message}))?;
            self.started = true;
        }
        Ok(())
    }
    fn text_block(&mut self, value: &Value) -> Result<usize, Failure> {
        let key = key(value)?;
        if let Some(index) = self.blocks.get(&key) {
            return Ok(*index);
        }
        let index = self.message["content"].as_array().unwrap().len();
        let block = json!({"type":"text","text":""});
        self.message["content"]
            .as_array_mut()
            .unwrap()
            .push(block.clone());
        self.blocks.insert(key, index);
        self.open.insert(index);
        self.send(&json!({"type":"content_block_start","index":index,"content_block":block}))?;
        Ok(index)
    }
    fn close(&mut self, index: usize) -> Result<(), Failure> {
        if self.open.remove(&index) {
            self.send(&json!({"type":"content_block_stop","index":index}))?;
        }
        Ok(())
    }

    fn tool_block(&mut self, value: &Value) -> Result<(), Failure> {
        let output_index = value["output_index"]
            .as_u64()
            .ok_or_else(|| Failure::upstream("Missing OpenAI output index"))?;
        let item = &value["item"];
        let field = |key| {
            item[key]
                .as_str()
                .ok_or_else(|| Failure::upstream("Invalid OpenAI function call"))
        };
        let id = field("id")?;
        let block =
            json!({"type":"tool_use","id":field("call_id")?,"name":field("name")?,"input":{}});
        let index = self.message["content"].as_array().unwrap().len();
        if self
            .calls
            .insert(output_index, (index, id.into()))
            .is_some()
        {
            return Err(Failure::upstream("Duplicate OpenAI function call"));
        }
        self.message["content"]
            .as_array_mut()
            .unwrap()
            .push(block.clone());
        self.open.insert(index);
        self.send(&json!({"type":"content_block_start","index":index,"content_block":block}))?;
        if let Some(arguments) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
            self.send(&json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":arguments}}))?;
        }
        Ok(())
    }

    fn tool_index(&self, value: &Value) -> Result<usize, Failure> {
        let output_index = value["output_index"]
            .as_u64()
            .ok_or_else(|| Failure::upstream("Missing OpenAI output index"))?;
        let item_id = value["item_id"]
            .as_str()
            .or_else(|| value["item"]["id"].as_str());
        self.calls
            .get(&output_index)
            .filter(|(_, id)| Some(id.as_str()) == item_id)
            .map(|(index, _)| *index)
            .ok_or_else(|| Failure::upstream("Unknown OpenAI function call"))
    }

    fn tool_input(&mut self, index: usize, arguments: &Value) -> Result<(), Failure> {
        let input = arguments
            .as_str()
            .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
            .filter(Value::is_object)
            .ok_or_else(|| Failure::upstream("OpenAI returned invalid function arguments"))?;
        if !self.streaming {
            self.message["content"][index]["input"] = input;
        }
        Ok(())
    }
}

fn key(value: &Value) -> Result<(u64, u64), Failure> {
    Ok((
        value["output_index"]
            .as_u64()
            .ok_or_else(|| Failure::upstream("Missing OpenAI output index"))?,
        value["content_index"]
            .as_u64()
            .ok_or_else(|| Failure::upstream("Missing OpenAI content index"))?,
    ))
}

fn read_event(reader: &mut impl BufRead) -> Result<Option<String>, Failure> {
    let mut data = Vec::new();
    let mut size = 0;
    loop {
        let mut line = String::new();
        let n = (&mut *reader)
            .take((1024 * 1024 - size + 1) as u64)
            .read_line(&mut line)
            .map_err(|_| Failure::upstream("Could not read the OpenAI stream"))?;
        size += n;
        if size > 1024 * 1024 {
            return Err(Failure::upstream("OpenAI stream event is too large"));
        }
        if n == 0 {
            return Ok(None);
        }
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            if !data.is_empty() {
                return Ok(Some(data.join("\n")));
            }
            size = 0;
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(
                value
                    .strip_prefix(' ')
                    .unwrap_or(value)
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
            );
        }
    }
}
