---
title: "接入 A2A Server"
description: "在管理控制台注册远程 Agent-to-Agent（A2A）端点,让它的 agent 被发现并可供你的 agent 委派调用。"
---

**A2A server** 是一个远程 agent 服务。注册它的端点后,Awaken 会拉取该 server 的
agent card,把它对外公布的 agent 转成 `AgentSpec`,并与你的本地 agent 一同列出 ——
可直接运行,也可被委派。本页介绍如何在浏览器里注册一个。

协议层契约(agent card 结构、`message:send`、task 轮询)见
[A2A 协议参考](/awaken/zh-cn/reference/protocols/a2a/)。

## 前置条件

- 一个已接入 `ConfigStore`、可从控制台访问的 `awaken-server`(见
  [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/))。
- 一个可达的远程 A2A 端点,在 `<base-url>/.well-known/agent-card.json` 提供
  agent card。

## 步骤

1. 在侧边栏(**Resources** 分组)打开 **A2A Servers**,点击 **New A2A server**。
2. 填写表单:
   - **Server ID**(必填)—— 该连接的稳定 id,创建后只读。
   - **Base URL**(必填)—— server 根地址,如 `https://agents.example.com`,
     agent card 相对于它读取。
   - **Timeout (ms)**(可选)—— 请求超时,1–30000,默认 10000。
   - **Optional target** —— 当 server 公布多个 agent/skill 时,固定指向其中一个。
   - **A2A bearer token**(可选)—— 发现与执行请求时携带。该字段有
     **Replace / Clear / Preserve** 三种模式,可轮换或保留已有密钥而无需重输。
   - **Options JSON**(可选)—— 适配器特定选项。

   <figure class="screenshot">
     <a href="/awaken/assets/admin-console/a2a-create.png">
       <img src="/awaken/assets/admin-console/a2a-create.png" alt="Create A2A server 表单:Server ID、Base URL、Timeout (ms)、Optional target、Options JSON,以及带 Replace/Clear/Preserve 模式的 A2A bearer token 字段。" loading="lazy" />
     </a>
     <figcaption>Create A2A server —— 只有 Server ID 与 Base URL 是必填。</figcaption>
   </figure>

3. 点击 **Refresh card** 立即拉取 agent card。表单会显示发现到的名称、版本、支持的
   接口、skills 以及连接状态。该操作调用 `GET /v1/a2a-servers/:id/status`(约 15s
   缓存)。
4. 点击 **Save**。控制台经一次非破坏性 `…/validate` 校验后,通过
   `POST /v1/config/a2a-servers`(编辑时为 `PUT /v1/config/a2a-servers/:id`)发布。

发现到的远程 agent 会出现在 **Agents** 列表中。打开你自己的某个 agent,在
**Delegates** 标签里把它们加入,即可让该 agent 委派给远程 agent。

## 控制台调用的端点

| 操作 | 端点 |
|---|---|
| 列出 / 获取 | `GET /v1/config/a2a-servers`、`GET /v1/config/a2a-servers/:id` |
| 创建 / 更新 | `POST /v1/config/a2a-servers`、`PUT /v1/config/a2a-servers/:id` |
| 校验(dry run) | `POST /v1/config/a2a-servers/validate` |
| 发现 / 状态 | `GET /v1/a2a-servers/:id/status` |
| 删除 | `DELETE /v1/config/a2a-servers/:id` |

## 注意

- **发现有防护。** server 拒绝从 loopback 与内网地址拉取 agent card(SSRF 防护),
  所以本地测试端点解析不到 card —— 请指向可路由的主机。
- **版本化配置。** 与所有配置资源一样,A2A server 带审计历史;用 **History** 标签
  查看或恢复历史 spec。

## 相关

- [A2A 协议参考](/awaken/zh-cn/reference/protocols/a2a/)
- [使用 Agent Handoff](/awaken/zh-cn/how-to/use-agent-handoff/)
- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)
