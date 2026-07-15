import { defineConfig } from 'vitepress'

const enNav = [
  {
    text: 'Guide',
    items: [
      { text: 'Getting Started', link: '/guide/getting-started' },
      { text: 'Configuration', link: '/guide/configuration' },
      { text: 'Eviction Policies', link: '/guide/eviction-policies' },
      { text: 'Dashboard', link: '/guide/dashboard' },
    ],
  },
  {
    text: 'Reference',
    items: [
      { text: 'Architecture', link: '/reference/architecture' },
      { text: 'Benchmarks', link: '/reference/benchmarks' },
    ],
  },
]

const enSidebar = [
  {
    text: 'Guide',
    items: [
      { text: 'Getting Started', link: '/guide/getting-started' },
      { text: 'Configuration', link: '/guide/configuration' },
      { text: 'Eviction Policies', link: '/guide/eviction-policies' },
      { text: 'Dashboard', link: '/guide/dashboard' },
    ],
  },
  {
    text: 'Reference',
    items: [
      { text: 'Architecture', link: '/reference/architecture' },
      { text: 'Benchmarks', link: '/reference/benchmarks' },
    ],
  },
]

const zhNav = [
  {
    text: '指南',
    items: [
      { text: '快速开始', link: '/zh/guide/getting-started' },
      { text: '配置', link: '/zh/guide/configuration' },
      { text: '淘汰策略', link: '/zh/guide/eviction-policies' },
      { text: '控制台', link: '/zh/guide/dashboard' },
    ],
  },
  {
    text: '参考',
    items: [
      { text: '架构', link: '/zh/reference/architecture' },
      { text: '性能基准', link: '/zh/reference/benchmarks' },
    ],
  },
]

export default defineConfig({
  title: 'FerrumKV',
  description: 'Eviction algorithm laboratory for RESP2-compatible KV stores.',
  lastUpdated: true,
  cleanUrls: true,
  locales: {
    root: {
      label: 'English',
      lang: 'en',
      themeConfig: {
        logo: '/logo.svg',
        nav: enNav,
        sidebar: enSidebar,
        socialLinks: [
          { icon: 'github', link: 'https://github.com/phaethix/ferrum-kv' },
        ],
      },
    },
    zh: {
      label: '简体中文',
      lang: 'zh-CN',
      themeConfig: {
        logo: '/logo.svg',
        nav: zhNav,
        sidebar: zhNav,
        socialLinks: [
          { icon: 'github', link: 'https://github.com/phaethix/ferrum-kv' },
        ],
      },
    },
  },
})
