# 12 · 多模态输入输出

> 回答疑问:**是的,μAgent 支持图片输入**,不止文本。
> 设计上把"文本 / 图像 / 音频 / PDF / 视频帧"统一成 `ContentPart`,`ModelAdapter::caps()` 返回的 `LlmCaps` 声明具体支持哪些模态。

## 12.1 统一的 Content 模型

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    Image(ImageRef),
    Audio(AudioRef),
    Pdf(PdfRef),
    Video(VideoRef),          // 帧抽样或短片
    Data { mime: String, bytes: InlineOrRef },   // 兜底
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ImageRef {
    pub data: InlineOrRef,            // 内联 bytes 或 URI
    pub mime: String,                 // "image/jpeg" / "image/png" / "image/heic"
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub thumb_b64: Option<String>,    // 64x64 缩略图,给审计/UI 用
    pub description: Option<String>,  // alt text,LLM 不支持视觉时用
}

#[derive(Clone, Serialize, Deserialize)]
pub enum InlineOrRef {
    Inline(Vec<u8>),       // 小于阈值(默认 256KB)直接嵌入
    Uri(String),           // 大文件引用,指向 fs:// 或临时 data://UUID
}
```

**为什么 `InlineOrRef`**:
- 小图(截图、缩略图)直接内联,provider 接收 base64。
- 大图走 URI,避免巨大 base64 塞进 store 和日志。
- Core 在发 provider 前按 backend 能力决定是否"物化"(inline 或 upload)。

**当前实现的硬约束**(2025+ codebase):
- `fs_read` 不会把 >1 MiB 的图片返回给模型 —— 直接 `ToolErr::deny` 带 hint
  "downsize first (e.g. convert to PNG at ≤1024x1024) and re-read"。1 MiB 选取
  覆盖典型截图 / 网图,同时把每张图保持在 ~10k tokens 以下(参见
  `IMAGE_TOKEN_COST` 估算)。
- 没有自动 resize:agent 自己用 `sh_exec` 跑 `ffmpeg`/`sips`/`convert` 等做
  预处理(host 决定哪些 binary 在 allowlist)。
- 12 MB HEIC 这类设备原图**不会**进 LLM 上下文,只能预处理后才进。
- 这条规则是**guardrail by default**:即使将来加自动压缩 pipeline,1 MiB cap
  仍是上限以保护 token 预算。

## 12.2 LlmCaps 声明

```rust
pub struct LlmCaps {
    pub native_tool_use: bool,
    pub json_schema_mode: bool,
    pub vision: bool,
    pub vision_max_images: Option<u32>,         // 单次请求最多几张
    pub vision_max_pixels: Option<u64>,         // 单张最大像素
    pub vision_formats: Vec<String>,            // ["image/jpeg","image/png"]
    pub audio_input: bool,
    pub audio_formats: Vec<String>,
    pub pdf_input: bool,                        // 原生 PDF(Claude 有)
    pub video_input: VideoSupport,              // None / Frames / Native
    pub thinking: bool,
    pub streaming: bool,
    pub ctx_len: u32,
    pub tokenizer: TokenizerHint,
}

pub enum VideoSupport { None, FrameSampled, Native }
```

ContextBuilder 在拼 LLM 请求时:
- 遇到 `Image` 但 `caps.vision == false` → 替换成 `Text(img.description.unwrap_or("[image omitted: vision not supported]"))`。
- 遇到 HEIC 但 `caps.vision_formats` 不含 → 经 Adapter 的 `ImageCodec` 转 JPEG。
- 超过 `vision_max_images` → 保留最新 N 张,其它转成 alt text。

## 12.3 图像 tokens 计入预算

图像 token 大致估算(provider 相关):
- Claude / GPT-4V:`~85 + 170 * tiles` 级别,1024×1024 大约 ~1200 tokens。
- Gemini:按分辨率档位。
- Apple FM:未公开,按 `(W*H)/512` 估。

```rust
impl TokenCounter {
    pub fn observe_image(&mut self, img: &ImageRef, hint: ImageTokenHint) -> u32 {
        let t = hint.estimate(img.width.unwrap_or(1024), img.height.unwrap_or(1024));
        self.count += t;
        t
    }
}
```

`ContextBuilder` 的预算裁剪:**图像优先级低于当前 turn 的 tool_results**,当预算吃紧时按"旧→新"丢图,每丢一张把 ImageRef 降级为 alt text("[image dropped from history, described as: ...]")。

## 12.4 多模态 tool 签名(宏自动识别)

```rust
#[tool(name = "vision.describe")]
/// 描述一张图片的内容。
async fn vision_describe(
    ctx: &ToolCtx<'_>,
    #[desc("图片 URI 或内联 base64")] image: ImageInput,
    #[default("zh")] language: String,
) -> Result<String, ToolErr> {
    // 这个 tool 本身会生成新的 LLM 请求,或转发给多模态 backend
    ...
}

#[tool(name = "camera.capture")]
/// 请求用户拍一张照片。
async fn camera_capture(
    ctx: &ToolCtx<'_>,
    #[default(1280)] max_dim: u32,
) -> Result<ImageRef, ToolErr> {
    ctx.adapters.camera.as_ref().ok_or(tool_err_nonretry("camera not available"))?
        .capture(CaptureOpts { max_dim }).await
        .map_err(|e| e.into_tool_err())
}
```

`ImageInput` 是宏识别的特殊类型:
```rust
pub enum ImageInput {
    B64 { data: String, mime: String },
    Uri(String),
}
```
Schema 自动加 oneOf,guard 层统一走 Adapter 的 `FileSystem::read` 或 base64 解码。

## 12.5 Tool 结果返回图像

Tool 可以返回图像(例如 `screen.capture`、`chart.render`、`photo.thumbnail`)。
`ToolOk` 扩展:

```rust
pub struct ToolOk {
    pub content: String,            // 给 LLM 的文字描述
    pub parts: Vec<ContentPart>,    // 额外多模态产物(可含 Image)
    pub detail: Option<Value>,
}
```

Core 在 push `ToolResult` 到 history 时,把 `parts` 和 `content` 合成一条 `ToolResult` message,下一 turn ContextBuilder 会把 image 传给 LLM(如果 caps.vision = true)。

**预算保护**:tool 返回图片 > `tool.max_out_bytes` 时,Adapter 的 `ImageCodec` 自动压缩(降分辨率到 `1024x1024 @ 85%` 或按 `ImageTokenHint` 目标 token)。

## 12.6 多模态的 Adapter(Shell 的扩展)

```rust
#[async_trait]
pub trait ImageCodec: Send + Sync {
    async fn decode(&self, bytes: &[u8]) -> Result<ImageMeta, CodecErr>;
    async fn encode_jpeg(&self, raw: &RawImage, quality: u8) -> Result<Vec<u8>, CodecErr>;
    async fn resize(&self, img: &RawImage, max_dim: u32) -> Result<RawImage, CodecErr>;
    async fn heic_to_jpeg(&self, bytes: &[u8]) -> Result<Vec<u8>, CodecErr>;
}

#[async_trait]
pub trait Camera: Send + Sync {
    async fn capture(&self, opts: CaptureOpts) -> Result<ImageRef, CameraErr>;
    async fn pick_from_library(&self, opts: PickOpts) -> Result<ImageRef, CameraErr>;
}

#[async_trait]
pub trait Microphone: Send + Sync {
    async fn record(&self, opts: RecordOpts) -> Result<AudioRef, MicErr>;
    async fn stream(&self) -> Result<AudioStream, MicErr>;
}

#[async_trait]
pub trait Screen: Send + Sync {
    async fn capture(&self, region: Option<ScreenRect>) -> Result<ImageRef, ScreenErr>;
}
```

`AdapterBundle` 里它们都是 `Option<Arc<dyn ...>>`,没有就不开相关 tool(三层门禁的 adapter-probe 环节会剔除)。

## 12.7 各平台实现要点

### iOS
- `ImageCodec`:CoreImage + ImageIO(HEIC/JPEG/PNG/RAW 都支持)。
- `Camera`:`UIImagePickerController` / 更现代 `PHPickerViewController`(隐私友好,无需相册权限)。
- `Microphone`:AVAudioEngine + 需 `NSMicrophoneUsageDescription`。
- `Screen`:**不可用**(iOS 不允许 app 截系统屏;仅可截自己 app 的视图,不实用)。
- `Photos` 来自 `PhotoKit`,`InterApp` 类别之一。

### Android
- `ImageCodec`:`BitmapFactory` + `ImageDecoder`(HEIC API29+)。
- `Camera`:CameraX。
- `Microphone`:MediaRecorder / AudioRecord。
- `Screen`:`MediaProjection`(需用户授权,可实现;企业 MDM 场景常用)。

### Linux/Pi
- `ImageCodec`:`image` crate。
- `Camera`:v4l2(Pi camera module / USB webcam)。
- `Screen`:`scrot` / `grim`(wayland) 走 `ProcessExec` allowlist。
- `Microphone`:ALSA / `cpal`。

## 12.8 输入链路(示例:用户从 iOS 相册选图发问)

```
用户点击 iOS app 的 "+"
 └─ app 调 PHPickerViewController
    └─ 得到 PHPickerResult → Data (HEIC 或 JPEG)
       └─ host 构造 UserMessage {
            Content::Parts(vec![
              Text("这张图里有什么植物?"),
              Image(ImageRef::inline(heic_bytes, "image/heic")),
            ])
          }
          └─ agent.run(message)
              └─ ContextBuilder:
                  - 检查 caps.vision / vision_formats
                  - 若 backend(如 Apple FM)不支持 HEIC → adapters.image_codec.heic_to_jpeg
                  - 若 backend(如老模型)不支持 vision → 改成 "[image: HEIC 12MB]"
                  - 估 tokens 进 budget
              └─ ModelAdapter.turn(request)  // 经 ModelRequest builder 统一进入
```

## 12.9 多模态输出流(UI)

Core 的 Event 枚举扩展变体(v3.1 Event 是 open-ended,multimodal 相关事件一起放在 Core/Shell 发出的统一 Event 里):
```rust
pub enum Event {
    ...
    AssistantPartDelta { part: PartDelta, seq: u64 },   // 不只 text
    ToolCallProducedImage { id: String, thumb_b64: String, uri: String, seq: u64 },
}

pub enum PartDelta {
    Text(String),
    ImageArriving { meta: ImageMeta, thumb_b64: String, uri: String },
    AudioArriving { meta: AudioMeta, uri: String },
}
```

UI 在 stream 过程中可以渐进渲染图像占位(thumb)和最终大图(uri)。

## 12.10 多模态与 Budget

```rust
pub struct ModalBudget {
    pub max_images_per_turn: u32,         // 默认 4
    pub max_image_dim: u32,               // 默认 1536
    pub total_image_tokens_in_ctx: u32,   // 默认 8000
    pub audio_sec_per_turn: u32,          // 默认 30
}
```

在 `RuntimeProfile` 里预置:
- `ios_default()`:max_images=2, max_image_dim=1024(省内存)。
- `raspberry_pi()`:max_images=8, max_image_dim=2048(Pi 4 能带动)。
- `mcu_minimal()`:多模态全关。

## 12.11 安全与隐私

| 风险 | 缓解 |
|---|---|
| 模型越权读取相册 | `camera.pick_from_library` 的 adapter 实现(iOS: `PHPickerViewController`)每次调用都让系统弹 picker;用户没选 = ToolResult::err 回 LLM |
| 图像中携带 EXIF 泄露 GPS | `ImageCodec` 默认剥离 EXIF(RuntimeProfile 可关闭) |
| 大图拖垮内存 | `max_image_dim` 强制压缩;inline 阈值控制 |
| 图像进审计日志 | 默认只存 `thumb_b64`(64×64) + 元数据,原图可选加密归档 |
| 录音被后台悄悄开启 | 录音期间必须显示 UI 指示(iOS 强制 orange dot) |

## 12.12 与 Skill 的协作

一些 Skill 天然多模态:
- `skill-ocr`:图片 → 文本(调 vision 或本地 Tesseract)。
- `skill-photo-album`:"找我上周拍的狗的照片"(元数据 + 视觉检索)。
- `skill-ui-auto`(Android):截屏 → 识别按钮 → 点击(Android MediaProjection + AccessibilityService)。
- `skill-voice`:麦克风 → on-device ASR → 文本 → agent → TTS。

每个 skill 的 `prompt_hint` 要明确告诉 LLM:
```
skill:photo-album -- access user's photo library
  Use photo.search to find photos by metadata (date/location/album).
  Use photo.export_thumb(id) to get a 256x256 preview.
  Use photo.export_full(id) only when user explicitly requests.
```

引导 LLM 先走 thumbnail + 元数据,避免把全分辨率图都拉进 context。
