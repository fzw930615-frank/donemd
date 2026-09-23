# M4 视频 Range 实测

下面是一个本地视频块，用于验证 `donemd-asset` 协议的 HTTP Range 支持（206 分段加载与 seek）。

<video controls src="./assets/clip.mp4"></video>

— 拖动进度条应即时跳转，不应整段缓冲。
