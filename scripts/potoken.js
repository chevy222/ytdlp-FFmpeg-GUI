// PO Token 生成器（Deno）。基于 MIT 许可的 bgutils-js 运行 YouTube BotGuard VM。
//
// 用法: deno run --allow-net --allow-env potoken.js [--content-binding <visitorData>] [--proxy <url>]
// 成功: stdout 输出单行 JSON {"visitorData":"...","poToken":"..."}
// 失败: stderr 输出 "PO_TOKEN_ERROR: ..."，exit 1
//
// 流程（与业界实现一致）：
//   1. GET youtube.com → 取 ytcfg（VISITOR_DATA / EVENT_ID）与 window.ytAtN 挑战
//   2. 加载 BotGuard 解释器脚本并执行 VM，snapshot 得到 botguardResponse
//   3. POST WAA GenerateIT 换取 integrityToken
//   4. WebPoMinter 用 integrityToken + visitorData 铸出 poToken
//
// 注意：GVS 场景下 poToken 与 visitorData 绑定，二者必须成对使用
// （调用方需把 visitorData 一并传给 yt-dlp：--extractor-args "youtube:visitor_data=..."）。
import { JSDOM, ResourceLoader } from "npm:jsdom@26";
import { BotGuardClient } from "npm:bgutils-js@4/botguard";
import { WebPoMinter } from "npm:bgutils-js@4/webpo";
import {
  buildURL,
  getHeaders,
  parseLooseJSON,
  USER_AGENT,
} from "npm:bgutils-js@4/utils";

// YouTube 多年沿用的公开 request key
const REQUEST_KEY = "O43z0dpjhgX20SCx4KAo";

function opt(name) {
  const i = Deno.args.indexOf(name);
  return i >= 0 && i + 1 < Deno.args.length ? Deno.args[i + 1] : undefined;
}

function fail(msg) {
  console.error("PO_TOKEN_ERROR: " + msg);
  Deno.exit(1);
}

const contentBindingArg = opt("--content-binding");
const proxy = opt("--proxy");

let httpClient;
const doFetch = (url, init = {}) =>
  fetch(url, httpClient ? { ...init, client: httpClient } : init);

// BotGuard VM 需要浏览器式全局环境（window/document/location/origin/navigator）
function setupDom() {
  const dom = new JSDOM(
    '<!DOCTYPE html><html lang="en"><head><title></title></head><body></body></html>',
    {
      url: "https://www.youtube.com/",
      referrer: "https://www.youtube.com/",
      resources: new ResourceLoader({ userAgent: USER_AGENT }),
    },
  );
  Object.assign(globalThis, {
    window: dom.window,
    document: dom.window.document,
    location: dom.window.location,
    origin: dom.window.origin,
  });
  if (!Reflect.has(globalThis, "navigator")) {
    Object.defineProperty(globalThis, "navigator", { value: dom.window.navigator });
  }
}

async function getChallengeAndVisitorData() {
  const resp = await doFetch("https://www.youtube.com", {
    headers: { "user-agent": USER_AGENT, "accept-language": "en-US,en;q=0.9" },
  });
  if (!resp.ok) throw new Error(`youtube.com HTTP ${resp.status}`);
  const html = await resp.text();

  const ytcfgMatch = html.match(/ytcfg\.set\(({.+?})\);/s);
  const ytcfg = ytcfgMatch ? JSON.parse(ytcfgMatch[1]) : {};
  const visitorData = contentBindingArg ||
    ytcfg.VISITOR_DATA ||
    (html.match(/"VISITOR_DATA":"([^"]+)"/) || [])[1];
  if (!visitorData) throw new Error("visitor_data not found");

  // BotGuard VM 读 yt.config_.EVENT_ID
  globalThis.yt = { config_: ytcfg };
  if (globalThis.window) globalThis.window.yt = globalThis.yt;

  let bgChallenge;
  const attMatch = html.match(/window\.ytAtN\(\s*({[\s\S]*?})\s*\)/);
  if (attMatch) bgChallenge = parseLooseJSON(attMatch[1])?.R?.bgChallenge;
  if (!bgChallenge) {
    // 退化：走 /att/get 拿挑战
    const attResp = await doFetch(
      "https://www.youtube.com/youtubei/v1/att/get?prettyPrint=false",
      {
        method: "POST",
        headers: { ...getHeaders(), "Content-Type": "application/json" },
        body: JSON.stringify({
          context: {
            client: { clientName: "WEB", clientVersion: "2.20260817.01.00" },
          },
          engagementType: "ENGAGEMENT_TYPE_UNBOUND",
        }),
      },
    );
    bgChallenge = (await attResp.json())?.bgChallenge;
  }
  if (!bgChallenge?.program || !bgChallenge?.interpreterUrl) {
    throw new Error("bgChallenge not found");
  }
  return { visitorData, bgChallenge };
}

async function generate() {
  setupDom();
  httpClient = proxy ? Deno.createHttpClient({ proxy: { url: proxy } }) : undefined;

  const { visitorData, bgChallenge } = await getChallengeAndVisitorData();

  const interpreterUrl =
    bgChallenge.interpreterUrl
      .privateDoNotAccessOrElseTrustedResourceUrlWrappedValue;
  const interpreterJs = await (await doFetch("https:" + interpreterUrl)).text();
  new Function(interpreterJs)();

  const bgClient = await BotGuardClient.create({
    program: bgChallenge.program,
    globalName: bgChallenge.globalName,
    globalObject: globalThis,
  });

  const webPoSignalOutput = [];
  const botguardResponse = await bgClient.snapshot({ webPoSignalOutput });

  const itResp = await doFetch(buildURL("GenerateIT"), {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify([REQUEST_KEY, botguardResponse]),
  });
  if (!itResp.ok) throw new Error(`GenerateIT HTTP ${itResp.status}`);
  const [integrityToken, estimatedTtlSecs, mintRefreshThreshold, websafeFallbackToken] =
    await itResp.json();
  if (!integrityToken) throw new Error("empty integrityToken");

  const minter = await WebPoMinter.create(
    { integrityToken, estimatedTtlSecs, mintRefreshThreshold, websafeFallbackToken },
    webPoSignalOutput,
  );
  const poToken = await minter.mintAsWebsafeString(visitorData);
  if (!poToken) throw new Error("empty poToken");

  console.log(
    JSON.stringify({ visitorData, poToken, ttlSecs: estimatedTtlSecs || 0 }),
  );
}

try {
  await generate();
} catch (e) {
  fail(e?.message || String(e));
}
