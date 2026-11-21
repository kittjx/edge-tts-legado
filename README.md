# edge-tts-legado

把微软 Edge「大声朗读」包装成 HTTP 接口，给 Legado(阅读) 的 HTTP 朗读引擎用。Rust + axum，依赖 `msedge-tts`（已适配 Sec-MS-GEC v2 签名）。

## Arch Linux 编译运行

```bash
sudo pacman -S --needed rust base-devel     # 或者用 rustup：sudo pacman -S rustup && rustup default stable
cd edge-tts-legado
cargo build --release                        # 首次约 1~2 分钟
PORT=3000 ./target/release/edge-tts-legado
```

可选环境变量：`PORT`（默认 3000）、`HOST`（默认 0.0.0.0）、`TOKEN`（设置后请求需带 `?token=xxx` 或 `Authorization: Bearer xxx`）。

## 本地测试

```bash
curl -s localhost:3000/health
curl -o test.mp3 "localhost:3000/tts?text=你好，这是一次测试&voice=zh-CN-XiaoxiaoNeural"
mpv test.mp3                                 # 或 ffplay / vlc
curl -s localhost:3000/voices | head -c 600  # 看可用发音人
```

## 接口

`GET|POST /tts`

| 参数 | 说明 |
|---|---|
| `text` | 要朗读的文本；也可以放在 POST 正文里（纯文本） |
| `voice` | 发音人，默认 `zh-CN-XiaoxiaoNeural` |
| `rate` | 语速 %，如 `-20`、`30`，默认 0 |
| `pitch` | 音调 Hz，默认 0 |
| `volume` | 音量 %，默认 0 |
| `format` | 默认 `audio-24khz-48kbitrate-mono-mp3` |

长文本会按句末标点自动切成约 1200 字一段，合成后拼接成一个 mp3 返回。

## Legado 配置

「朗读引擎 → 新建/导入」，按 HTTP 朗读引擎填写。手机和电脑在同一局域网时，把地址换成电脑的局域网 IP（`ip addr` 查看），并放行 3000 端口。

POST 写法（推荐，文本不受 URL 长度限制）：

```
http://电脑IP:3000/tts?voice=zh-CN-XiaoxiaoNeural&rate=0,{"method":"POST","body":"{{speakText}}"}
```

GET 写法：

```
http://电脑IP:3000/tts?voice=zh-CN-XiaoxiaoNeural&rate=0&text={{java.encodeURI(speakText)}}
```

Content-Type 填 `audio/mpeg`。如果要跟随 Legado 的语速设置，把 `rate` 换成 `{{speakSpeed}}` 的换算表达式，具体换算看你 Legado 版本里语速的取值范围。

## Docker / Compose

```bash
cp .env.example .env          # 按需修改 HOST_PORT、CONCURRENCY、TOKEN
docker compose up -d --build
docker compose logs -f        # 看日志
curl localhost:3000/health
```

镜像基于 alpine（musl 静态编译），运行时以非 root 用户运行。参数都可以在 `.env` 或 `compose.yaml` 的 `environment` 里改：

| 变量 | 说明 | 默认 |
|---|---|---|
| `HOST_PORT` | 宿主机映射端口（容器内固定 3000） | 3000 |
| `CONCURRENCY` | 同时向微软发起的合成请求数 | 2 |
| `TOKEN` | 访问令牌，留空不鉴权 | 空 |
