// PO Token 生成器（deno）
// 用法: deno run --allow-net potoken.js
// 成功: stdout 输出纯文本 token
// 失败: stderr 输出错误，exit 1
//
// 生成流程：
// 1. GET youtube.com 提取 visitor_data
// 2. POST youtubei/v1/verify 获取 PO Token
// 基于 yt-dlp 社区公开的端点与参数，YouTube 调整端点时需更新此脚本。

const API_KEY = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";
const UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

async function fetchVisitorData() {
  const resp = await fetch("https://www.youtube.com/", {
    headers: {
      "User-Agent": UA,
      "Accept-Language": "en-US,en;q=0.9",
    },
    redirect: "follow",
  });
  if (!resp.ok) throw new Error(`youtube.com HTTP ${resp.status}`);
  const html = await resp.text();
  // ytcfg 里的 VISITOR_DATA
  let m = html.match(/"VISITOR_DATA":"([^"]+)"/);
  if (m) return m[1];
  // 退化：从 cookie 读 VISITOR_INFO1_LIVE 当 visitorData（部分场景可用）
  const setCookie = resp.headers.get("set-cookie") || "";
  const cm = setCookie.match(/VISITOR_INFO1_LIVE=([^;]+)/);
  if (cm) return cm[1];
  throw new Error("visitor_data not found in youtube.com response");
}

async function generatePoToken(visitorData) {
  const resp = await fetch(
    `https://www.youtube.com/youtubei/v1/verify?key=${API_KEY}`,
    {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "User-Agent": UA,
        "Origin": "https://www.youtube.com",
        "Referer": "https://www.youtube.com/",
        "Accept-Language": "en-US,en;q=0.9",
      },
      body: JSON.stringify({
        context: {
          client: {
            clientName: "WEB",
            clientVersion: "2.20240501.00.00",
            visitorData,
            hl: "en",
            gl: "US",
          },
        },
      }),
    }
  );
  const text = await resp.text();
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    throw new Error(`verify endpoint non-JSON (HTTP ${resp.status}): ${text.slice(0, 200)}`);
  }
  // YouTube 返回字段可能是 poToken / po_token / token
  const token = data.poToken || data.po_token || data.token;
  if (!token) {
    throw new Error(
      `PO Token not in verify response: ${JSON.stringify(data).slice(0, 300)}`
    );
  }
  return token;
}

try {
  const vd = await fetchVisitorData();
  const token = await generatePoToken(vd);
  console.log(token);
} catch (e) {
  console.error("PO_TOKEN_ERROR: " + e.message);
  Deno.exit(1);
}
