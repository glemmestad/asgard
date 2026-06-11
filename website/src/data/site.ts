/* ============================================================
   Single source of truth for site copy.
   Edit here, the page sections render from these values.
   ============================================================ */

export const site = {
  name: 'Frontkeep',
  tagline: 'The Agent Control Plane.',
  domain: 'asgard.build',
  github: 'https://github.com/glemmestad/asgard',
  docs: '/docs',
  contact: 'mailto:hello@asgard.build',
};

export const nav = [
  { label: 'How it works', href: '#loop' },
  { label: 'Platform', href: '#platform' },
  { label: 'Agents', href: '#agents' },
  { label: 'Open Core', href: '#open-core' },
  { label: 'Docs', href: site.docs },
];

/* The four-move governed onboarding loop, the headline workflow. */
export const loop = [
  {
    n: '01',
    key: 'seed',
    title: 'Read the standards',
    body: 'Point a new effort’s agent at the seed repo. It reads your company’s standards, security rules, and workflow over MCP, the authoritative, versioned copy, not a stale snapshot.',
    tool: 'get_standards',
  },
  {
    n: '02',
    key: 'register',
    title: 'Register the project',
    body: 'The mandatory gate. Nothing provisions or spends until a project is registered and active. Registration mints a stable proj-YYYY-NNNN id and records owner, manager, group and classification.',
    tool: 'register_project',
  },
  {
    n: '03',
    key: 'provision',
    title: 'Provision through the orchestrator',
    body: 'Registration unlocks gateway keys and real services. Cheap, reversible resources auto-provision; high blast-radius ones route for approval. Every resource is tagged with the project, and access between resources is itself a governed request, so one service reaches another only when the grant is filed, scoped and audited.',
    tool: 'request_resource',
  },
  {
    n: '04',
    key: 'cost',
    title: 'Attribute every dollar',
    body: 'Model spend and infrastructure spend both carry owner / manager / group / classification on every row, so cost rolls up by any dimension with a single query, all the way to wide production use.',
    tool: 'cost_report',
  },
];

/* The platform pillars. */
export const pillars = [
  {
    icon: 'catalog',
    title: 'Manifest-driven catalog',
    body: 'Everything an agent can provision is one YAML per service. Adding a service is dropping a manifest plus a Terraform module, no recompile. The terraform connector is the universal path to any cloud.',
  },
  {
    icon: 'gate',
    title: 'The registry gate',
    body: 'No keys, no provisioning, no spend until a project is registered. Classification tiers, evidence-gated promotion, and review-date sweeps run the whole lifecycle from POC to wide production.',
  },
  {
    icon: 'gateway',
    title: 'Governed model gateway',
    body: 'Every model call routes through it: per-project virtual keys, budgets, model allowlists per data class, PII / secret / prompt-injection guardrails, a kill switch, and full audit.',
  },
  {
    icon: 'policy',
    title: 'One policy engine',
    body: 'A single Cedar engine queried by the gateway, catalog, workflow and runtime: can this principal do this, against this data class, with this model, and does it need approval, from whom?',
  },
  {
    icon: 'cost',
    title: 'Cost attribution',
    body: 'Model and infra spend denormalize the same dimensions, so attribution is a plain query. Daily rollups, month-to-date deltas, an EOM forecast, an org cost tree and a governed cost Q&A.',
  },
  {
    icon: 'knowledge',
    title: 'Knowledge platform',
    body: 'Normative standards, advisory guidance and composable recipes, versioned with an edit trail and per-version diff, full-text search and moderation. Served to humans in the UI and to agents over MCP.',
  },
];

/* MCP tools the control plane exposes to agents. */
export const mcpTools = [
  'list_standards',
  'get_standards',
  'register_project',
  'catalog_search',
  'catalog_get',
  'gateway_credential',
  'request_resource',
  'request_grant',
  'mcp_catalog_publish',
  'request_promotion',
  'cost_report',
];

/* Open-core split. */
export const openCore = {
  oss: {
    label: 'Open source',
    price: 'Apache 2.0',
    blurb: 'The entire governance spine. Self-host the single binary, forever, for free.',
    features: [
      'Manifest-driven service catalog + Terraform connector',
      'Registry gate, classification tiers & lifecycle',
      'Model gateway, OpenAI & Anthropic built in, any OpenAI-compatible backend as a plugin',
      'Cedar policy engine & full audit trail',
      'Cost attribution, rollups, org tree & forecast',
      'Knowledge platform: standards, guidance, recipes',
      'MCP control plane (stdio + remote) + CLI + REST/GraphQL + embedded UI',
      'Basic OIDC sign-in',
      'SQLite by default, Postgres opt-in',
    ],
    cta: { label: 'Get started', href: '/docs' },
  },
  enterprise: {
    label: 'Enterprise',
    price: 'Licensed',
    blurb: 'For regulated, multi-team rollouts. Sits behind clean trait seams in the same binary.',
    features: [
      'Advanced identity, SAML & SCIM provisioning, beyond basic OIDC',
      'High availability, multi-instance, coordinated deployment',
      'SIEM / audit streaming',
      'Multi-tenant isolation',
      'Priority support & SLAs',
    ],
    cta: { label: 'Register interest', href: 'mailto:hello@asgard.build?subject=Frontkeep%20Enterprise' },
    note: 'Enterprise licensing isn’t generally available yet. Contact us to register interest.',
  },
};

export const faqs = [
  {
    q: 'What about the agent work already happening on my teams?',
    a: 'Exactly. People are already pointing agents at real problems and shipping software faster than any roadmap can. Frontkeep doesn’t stop the momentum — it gives it a workplace, so the same work reads your standards, registers the project, gets scoped credentials, and shows up attributed and auditable instead of on a mystery bill.',
  },
  {
    q: 'Isn’t this just an LLM gateway?',
    a: 'No. The gateway is one component. Frontkeep is the front door you point agents at for the whole journey, reading standards, registering the project, provisioning real infrastructure, and attributing cost, from the first prompt all the way to wide production use across the company.',
  },
  {
    q: 'How do agents use it?',
    a: 'Agents-first by design. Every capability is an MCP tool, not just a UI button. Connect Claude Code, Cursor, or Codex to the remote MCP server at /mcp with a personal access token or a per-project key.',
  },
  {
    q: 'How is it deployed?',
    a: 'One static Rust binary with an embedded UI. docker run frontkeep and you’re productive in an afternoon. SQLite needs zero external services; switch the database URL to Postgres and add stateless replicas to scale out. Kubernetes is supported, never required.',
  },
  {
    q: 'How do we add our own services?',
    a: 'Drop a service.yaml manifest and a Terraform module, no recompile. The manifest declares how the service is provisioned and how its cost is attributed. Non-AWS providers use the exact same path.',
  },
  {
    q: 'Can teams share their own MCP servers?',
    a: 'Yes. Frontkeep ships an MCP catalog: a publishable registry of MCP servers, kept separate from the provisioning catalog because it’s opt-in sharing, not derived from what a project built. Company-approved and user-submitted servers sit side by side, each with an owner as contact and a ready-to-paste install snippet for Claude Code, Codex or Cursor. An agent browses it over MCP; a user publishes one with their token.',
  },
];
