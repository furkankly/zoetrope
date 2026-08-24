// The landing page's copy, one table per language. `index.astro` (en) and
// `zh/index.astro` (zh) render the same Landing component against their table.
// Keys with markup (<em>, <br/>) are interpolated into the template as-is.

export interface LandingCopy {
  htmlLang: string;
  title: string;
  description: string;
  nav: { keys: string; docs: string; github: string; openApp: string; langSwitch: string; langSwitchHref: string };
  hero: {
    artLabel: string;
    h1Top: string; // first line(s), before the gold <em>
    h1Em: string; // the gold word
    sub: string;
    tryBrowser: string;
    installTui: string;
    fineprint: string;
  };
  tiles: { glyph: string; cls: 'run' | 'gold'; h: string; p: string }[];
  shots: { head: string; videoLabel: string; cap: string }[];
  shotnotePre: string;
  shotnoteLink: string;
  shotnotePost: string;
  close: { h2: string; openApp: string; install: string };
  foot: { tagline: string; built: string };
}

export const LANDING: Record<'en' | 'zh', LandingCopy> = {
  en: {
    htmlLang: 'en',
    title: 'zoetrope — watch your agents work',
    description:
      'A terminal UI that visualizes Claude Code agent sessions as a live flow graph — replay a finished transcript or follow a running one. Zero network IO; nothing leaves your machine.',
    nav: {
      keys: 'keys',
      docs: 'docs',
      github: 'github ↗',
      openApp: 'open app',
      langSwitch: '中文',
      langSwitchHref: '/zh/',
    },
    hero: {
      artLabel: 'A zoetrope flow graph showing a main agent, its subagents, and their live tool activity.',
      h1Top: 'watch your<br />agents ',
      h1Em: 'work.',
      sub: 'Claude Code writes a JSONL log for every session. zoetrope reads it and lays the session out as a graph: which agents ran, and what each one is doing. Watch a live session, or replay a finished one.',
      tryBrowser: '▸ try it in your browser',
      installTui: 'install the TUI',
      fineprint: 'reads the JSONL Claude Code already writes · runs fully local.',
    },
    tiles: [
      {
        glyph: '●',
        cls: 'run',
        h: 'the whole tree',
        p: 'Every agent and the subagents it spawns, on one graph. See who kicked off what, and what each is running now.',
      },
      {
        glyph: '⚒',
        cls: 'gold',
        h: 'live tool activity',
        p: 'See each tool call as an agent makes it, and whether it passed or failed.',
      },
      {
        glyph: '◆',
        cls: 'gold',
        h: 'time-travel any session',
        p: 'Scrub back, pause, jump between prompts, or snap back to live. The timeline is spaced by how busy the session was, not by the clock.',
      },
      {
        glyph: '↳',
        cls: 'gold',
        h: 'full provenance',
        p: 'Click any agent to see why it ran: the prompt that spawned it, and the reasoning behind it.',
      },
    ],
    shots: [
      {
        head: 'the whole session · overview',
        videoLabel: 'zoetrope replaying a Claude Code session as a flow graph, the whole tree in view',
        cap: 'One agent fans out to four subagents and a review workflow. The timeline fills as it goes — a ✗ marks the test run that failed before it was fixed.',
      },
      {
        head: 'following the work · live camera',
        videoLabel: 'zoetrope in follow mode, the camera tracking whichever agent is working',
        cap: 'Hand the camera to the action (<kbd>f</kbd>) and it glides to whichever agent just did something, at a readable zoom. This is what watching a live session looks like.',
      },
      {
        head: 'using it · pan, zoom, inspect, scrub',
        videoLabel: 'Panning, zooming, opening an agent’s detail panel, and dragging the scrubber to time-travel',
        cap: 'Click an agent for its provenance — the prompt that spawned it, the reasoning, every tool it ran. Then drag the scrubber to travel back through the session.',
      },
    ],
    shotnotePre: 'The same engine compiles to WebAssembly —',
    shotnoteLink: 'replay this exact session in your browser',
    shotnotePost: ', no install.',
    close: {
      h2: 'see what your agents are doing.',
      openApp: '▸ open the app',
      install: 'install the terminal app',
    },
    foot: {
      tagline: 'replay finished runs · follow live ones',
      built: 'built on',
    },
  },
  zh: {
    htmlLang: 'zh-CN',
    title: 'zoetrope — 看你的 agent 干活',
    description:
      '把 Claude Code agent 会话可视化为实时流程图的终端 UI——回放已结束的 transcript，或实时跟随正在运行的会话。零网络 IO，数据不出本机。',
    nav: {
      keys: '按键',
      docs: '文档',
      github: 'github ↗',
      openApp: '打开应用',
      langSwitch: 'En',
      langSwitchHref: '/',
    },
    hero: {
      artLabel: '一张 zoetrope 流程图：主 agent、它的子 agent，以及它们的实时工具活动。',
      h1Top: '看你的<br />agent ',
      h1Em: '干活。',
      sub: 'Claude Code 会为每个会话写一份 JSONL 日志。zoetrope 读取它，把会话铺成一张图：哪些 agent 跑过，各自正在做什么。看一个实时会话，或回放一个已结束的。',
      tryBrowser: '▸ 在浏览器里试试',
      installTui: '安装终端版',
      fineprint: '读取 Claude Code 本来就在写的 JSONL · 完全本地运行。',
    },
    tiles: [
      {
        glyph: '●',
        cls: 'run',
        h: '完整的树',
        p: '每个 agent 和它派生的子 agent 都在一张图上。看得出谁发起了什么、各自正在运行什么。',
      },
      {
        glyph: '⚒',
        cls: 'gold',
        h: '实时工具活动',
        p: 'agent 每发起一次工具调用都看得见，成功还是失败一目了然。',
      },
      {
        glyph: '◆',
        cls: 'gold',
        h: '任意会话可时间旅行',
        p: '回拖、暂停、在提示词之间跳转，或一键回到直播。时间线的疏密按会话的忙碌程度排布，而不是按钟表。',
      },
      {
        glyph: '↳',
        cls: 'gold',
        h: '完整溯源',
        p: '点击任意 agent，看它为何而运行：派生它的提示词，以及背后的思考。',
      },
    ],
    shots: [
      {
        head: '整个会话 · 总览',
        videoLabel: 'zoetrope 将 Claude Code 会话回放为流程图，全树尽收眼底',
        cap: '一个 agent 扇出四个子 agent 和一个评审工作流。时间线随之填满——✗ 标出了修复前失败的那次测试。',
      },
      {
        head: '跟随工作 · 实时镜头',
        videoLabel: 'zoetrope 处于跟随模式，镜头追踪正在工作的 agent',
        cap: '把镜头交给动作（<kbd>f</kbd>），它会以可读的缩放滑向刚刚有所动作的 agent。这就是看实时会话的样子。',
      },
      {
        head: '操作 · 平移、缩放、查看、拖拽',
        videoLabel: '平移、缩放、打开 agent 详情面板、拖动进度条进行时间旅行',
        cap: '点击一个 agent 看它的溯源——派生它的提示词、背后的思考、跑过的每一个工具。然后拖动进度条回到会话的过去。',
      },
    ],
    shotnotePre: '同一套引擎可编译为 WebAssembly——',
    shotnoteLink: '在浏览器里回放这个一模一样的会话',
    shotnotePost: '，无需安装。',
    close: {
      h2: '看看你的 agent 在做什么。',
      openApp: '▸ 打开应用',
      install: '安装终端版',
    },
    foot: {
      tagline: '回放已结束的运行 · 跟随正在进行的',
      built: '基于',
    },
  },
};
