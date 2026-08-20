use rquickjs::{
    class::Trace,
    function::Opt,
    Class, Ctx, Error, JsLifetime, Object, Result, TypedArray, Value,
};

fn throw_range_error<'js>(ctx: &Ctx<'js>, msg: String) -> Error {
    let val: Value = ctx
        .eval::<Value<'js>, _>(format!(
            "new RangeError({})",
            serde_json::to_string(&msg).unwrap_or_else(|_| "\"RangeError\"".into())
        ))
        .unwrap_or_else(|_| ctx.eval("new RangeError('RangeError')").unwrap());
    ctx.throw(val)
}

fn throw_type_error<'js>(ctx: &Ctx<'js>, msg: &str) -> Error {
    let val: Value = ctx
        .eval::<Value<'js>, _>(format!(
            "new TypeError({})",
            serde_json::to_string(msg).unwrap_or_else(|_| "\"TypeError\"".into())
        ))
        .unwrap_or_else(|_| ctx.eval("new TypeError('TypeError')").unwrap());
    ctx.throw(val)
}

fn extract_bytes<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Option<Vec<u8>>> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    if let Some(ta) = value.as_object().and_then(|o| o.as_typed_array::<u8>()) {
        if let Some(bytes) = ta.as_bytes() {
            return Ok(Some(bytes.to_vec()));
        }
        return Ok(Some(Vec::new()));
    }
    if let Some(ab) = value.as_object().and_then(|o| o.as_array_buffer()) {
        if let Some(bytes) = ab.as_bytes() {
            return Ok(Some(bytes.to_vec()));
        }
        return Ok(Some(Vec::new()));
    }
    // Try DataView or other ArrayBufferView: look for buffer, byteOffset, byteLength
    if let Some(obj) = value.as_object() {
        if let Ok(buffer) = obj.get::<_, Value>("buffer") {
            if let Some(ab) = buffer.as_object().and_then(|o| o.as_array_buffer()) {
                if let Some(all_bytes) = ab.as_bytes() {
                    let offset: usize = obj.get::<_, usize>("byteOffset").unwrap_or(0);
                    let len: usize = obj.get::<_, usize>("byteLength").unwrap_or(0);
                    if offset + len <= all_bytes.len() {
                        return Ok(Some(all_bytes[offset..offset + len].to_vec()));
                    }
                }
            }
            if let Some(ta) = buffer.as_object().and_then(|o| o.as_typed_array::<u8>()) {
                if let Some(all_bytes) = ta.as_bytes() {
                    let offset: usize = obj.get::<_, usize>("byteOffset").unwrap_or(0);
                    let len: usize = obj.get::<_, usize>("byteLength").unwrap_or(0);
                    if offset + len <= all_bytes.len() {
                        return Ok(Some(all_bytes[offset..offset + len].to_vec()));
                    }
                }
            }
        }
        // Fallback: try to coerce via JS `new Uint8Array(value)` if it's an array-like
        // For string input, spec says BufferSource only, but be lenient and try
        if value.is_string() {
            return Err(throw_type_error(
                ctx,
                "Failed to execute 'decode' on 'TextDecoder': The provided value is not of type '(ArrayBuffer or ArrayBufferView)'",
            ));
        }
    }
    // Try coercing via JS: if it's an object with length, try Uint8Array
    // Last resort: try to get as TypedArray via from_js which will error if not suitable
    Err(throw_type_error(
        ctx,
        "Failed to execute 'decode' on 'TextDecoder': The provided value is not of type '(ArrayBuffer or ArrayBufferView)'",
    ))
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "TextDecoder")]
pub struct JsTextDecoder {
    encoding_name: String,
    fatal: bool,
    ignore_bom: bool,
}

#[rquickjs::methods]
impl JsTextDecoder {
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        label: Opt<String>,
        options: Opt<Object<'js>>,
    ) -> Result<Self> {
        let label_str = label.0.unwrap_or_else(|| "utf-8".to_string());
        let canonical = tinybrowser_net::TextDecoder::new(
            &label_str,
            tinybrowser_net::TextDecoderOptions::default(),
        )
        .map(|d| d.encoding_name().to_ascii_lowercase())
        .unwrap_or_default();
        if canonical.is_empty() {
            return Err(throw_range_error(
                &ctx,
                format!(
                    "Failed to construct 'TextDecoder': The encoding label provided ('{}') is invalid.",
                    label_str
                ),
            ));
        }
        let mut fatal = false;
        let mut ignore_bom = false;
        if let Some(opts) = options.0 {
            if let Ok(v) = opts.get::<_, bool>("fatal") {
                fatal = v;
            }
            if let Ok(v) = opts.get::<_, bool>("ignoreBOM") {
                ignore_bom = v;
            }
        }
        Ok(Self {
            encoding_name: canonical,
            fatal,
            ignore_bom,
        })
    }

    #[qjs(get, rename = "encoding")]
    pub fn encoding(&self) -> String {
        self.encoding_name.clone()
    }

    #[qjs(get, rename = "fatal")]
    pub fn fatal(&self) -> bool {
        self.fatal
    }

    #[qjs(get, rename = "ignoreBOM")]
    pub fn ignore_bom(&self) -> bool {
        self.ignore_bom
    }

    pub fn decode<'js>(
        &self,
        ctx: Ctx<'js>,
        input: Opt<Value<'js>>,
        options: Opt<Object<'js>>,
    ) -> Result<String> {
        let _stream = options
            .0
            .as_ref()
            .and_then(|o| o.get::<_, bool>("stream").ok())
            .unwrap_or(false);

        let bytes_opt = match input.0 {
            None => None,
            Some(v) => extract_bytes(&ctx, v)?,
        };
        let bytes = match bytes_opt {
            None => return Ok(String::new()),
            Some(b) => b,
        };

        if self.fatal {
            let opts = tinybrowser_net::TextDecoderOptions {
                fatal: true,
                ignore_bom: self.ignore_bom,
            };
            let decoder = tinybrowser_net::TextDecoder::new(&self.encoding_name, opts)
                .expect("encoding already validated");
            match decoder.decode(&bytes) {
                Some(s) => Ok(s),
                None => Err(throw_type_error(
                    &ctx,
                    "Failed to execute 'decode' on 'TextDecoder': The encoded data was not valid.",
                )),
            }
        } else {
            // For non-fatal, use encoding_rs::Encoding::decode directly — it correctly handles
            // single trailing invalid bytes (e.g. [0xFF] -> U+FFFD) where the incremental
            // decoder used by tinybrowser_net::TextDecoder currently returns "" for that case.
            // BOM handling: Encoding::decode strips UTF-8 BOM; when ignoreBOM is true we must keep it.
            let encoding = encoding_rs::Encoding::for_label(self.encoding_name.as_bytes())
                .expect("encoding already validated");
            if self.ignore_bom
                && encoding == encoding_rs::UTF_8
                && bytes.len() >= 3
                && bytes[0] == 0xEF
                && bytes[1] == 0xBB
                && bytes[2] == 0xBF
            {
                let (cow, _, _) = encoding.decode(&bytes[3..]);
                let mut s = String::with_capacity(cow.len() + 1);
                s.push('\u{FEFF}');
                s.push_str(&cow);
                Ok(s)
            } else {
                let (cow, _, _) = encoding.decode(&bytes);
                Ok(cow.into_owned())
            }
        }
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "TextEncoder")]
pub struct JsTextEncoder {}

#[rquickjs::methods]
impl JsTextEncoder {
    #[qjs(constructor)]
    pub fn new() -> Self {
        Self {}
    }

    #[qjs(get, rename = "encoding")]
    pub fn encoding(&self) -> String {
        "utf-8".to_string()
    }

    pub fn encode<'js>(&self, ctx: Ctx<'js>, input: Opt<String>) -> Result<TypedArray<'js, u8>> {
        let s = input.0.unwrap_or_default();
        let bytes = s.as_bytes().to_vec();
        TypedArray::new(ctx, bytes)
    }

    #[qjs(rename = "encodeInto")]
    pub fn encode_into<'js>(
        &self,
        ctx: Ctx<'js>,
        source: Opt<String>,
        dest: TypedArray<'js, u8>,
    ) -> Result<Object<'js>> {
        let s = source.0.unwrap_or_default();
        let dest_len = dest.len();
        let obj = Object::new(ctx.clone())?;

        if dest_len == 0 {
            obj.set("read", 0)?;
            obj.set("written", 0)?;
            return Ok(obj);
        }

        // Encode char by char to avoid splitting multi-byte sequences when dest is too small.
        let mut written = 0usize;
        let mut read_utf16 = 0usize;
        let mut read_chars = 0usize;

        for c in s.chars() {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            let b_len = encoded.len();
            if written + b_len > dest_len {
                break;
            }
            written += b_len;
            read_chars += 1;
            read_utf16 += c.len_utf16();
        }

        if written == s.as_bytes().len() {
            read_utf16 = s.encode_utf16().count();
        }

        // Copy the prefix bytes into dest via JS — safe, no unsafe, and correctly binds `this`.
        if written > 0 {
            let prefix: String = s.chars().take(read_chars).collect();
            let prefix_bytes = prefix.as_bytes().to_vec();
            let src_ta = TypedArray::new(ctx.clone(), prefix_bytes)?;
            ctx.globals().set("__tb_tmp_src", src_ta)?;
            ctx.globals().set("__tb_tmp_dest", dest.clone())?;
            ctx.eval::<(), _>("__tb_tmp_dest.set(__tb_tmp_src, 0)")
                .map_err(|e| {
                    let _ = ctx.eval::<(), _>("delete globalThis.__tb_tmp_src; delete globalThis.__tb_tmp_dest;");
                    e
                })?;
            ctx.eval::<(), _>("delete globalThis.__tb_tmp_src; delete globalThis.__tb_tmp_dest;")?;
        }

        obj.set("read", read_utf16)?;
        obj.set("written", written)?;
        Ok(obj)
    }
}

pub fn register_text_codec<'js>(ctx: &Ctx<'js>) -> Result<()> {
    Class::<JsTextDecoder>::define(&ctx.globals())?;
    Class::<JsTextEncoder>::define(&ctx.globals())?;

    // TextDecoderStream / TextEncoderStream remain JS wrappers around the native classes
    // (bootstrap.js polyfills will now delegate to native TextDecoder/Encoder).
    // Define them natively as thin wrappers for completeness, or leave to JS.
    // For now, we leave Streams to JS; they will auto-use native TextDecoder/Encoder.

    Ok(())
}
