// @ts-check
/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'Asgard',
  tagline: 'Open-source control plane for AI & agent development',
  url: 'https://asgard.dev',
  // Served by the Asgard binary under /docs (embedded via rust-embed), so every
  // asset and route is prefixed accordingly.
  baseUrl: '/docs/',
  onBrokenLinks: 'warn',
  onBrokenMarkdownLinks: 'warn',
  i18n: { defaultLocale: 'en', locales: ['en'] },
  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          routeBasePath: '/',
          sidebarPath: require.resolve('./sidebars.js'),
        },
        blog: false,
        theme: {},
      }),
    ],
  ],
  themeConfig: {
    navbar: {
      title: 'Asgard',
      items: [
        { type: 'docSidebar', sidebarId: 'docs', position: 'left', label: 'Docs' },
        { href: '/', label: '← App', position: 'right' },
      ],
    },
    footer: {
      style: 'dark',
      copyright: 'Apache-2.0. Asgard.',
    },
  },
};

module.exports = config;
