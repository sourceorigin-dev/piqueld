import { test as base, expect, type Browser, type CDPSession, type Locator, type Page, type Route, type TestInfo } from '@playwright/test';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const bin = process.env.PIQUELD_PLAYWRIGHT_BIN_DIR ?? resolve(root, process.env.CARGO_TARGET_DIR ?? 'target', 'debug');
export type User = { id: string; username: string };
export type Grant = { permission: string; applications?: string[] };
export type Session = { user: User; grants: Grant[]; scoped: boolean };
export type Account = { user: User; grants: Grant[] };
export type Ceremony = { id: string; options: { publicKey: unknown } };
export type Directory = { users: Account[] };
export type Managed = { invitation_url: string; token: string };
export type Daemon = { origin: string; setup: string; socket: string; directory: string };

/** One owned child process, with bounded shutdown and output kept for diagnostics. */
export class Process {
  readonly child: ChildProcessWithoutNullStreams;
  stdout = '';
  stderr = '';
  readonly exited: Promise<number | null>;
  constructor(command: string, args: string[], env: NodeJS.ProcessEnv = process.env, cwd?: string) {
    this.child = spawn(command, args, { env, cwd, stdio: 'pipe' });
    this.child.stdout.on('data', chunk => { this.stdout += chunk; });
    this.child.stderr.on('data', chunk => { this.stderr += chunk; });
    this.exited = new Promise((done, reject) => {
      this.child.once('error', reject);
      this.child.once('close', done);
    });
    // Startup failures are reported by waitForOutput/run without unhandled rejections.
    void this.exited.catch(() => {});
  }
  async waitForOutput(pattern: RegExp, stream: 'stdout' | 'stderr'): Promise<string> {
    await expect.poll(() => {
      if (this.child.exitCode !== null) {
        throw new Error(`Process exited before readiness: ${this.stdout}\n${this.stderr}`);
      }
      return this[stream].match(pattern)?.[1];
    }, { timeout: 15_000, message: `Waiting for ${pattern}` }).toBeTruthy();
    return this[stream].match(pattern)![1];
  }
  async stop() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill('SIGKILL'), 3_000);
    try { await this.exited; } finally { clearTimeout(timer); }
  }
}

/** How long before the test's timeout `Cli.run` kills a command, so the
 * test fails with the command and its output rather than a bare timeout. */
const commandMargin = 5_000;

/** `piquelctl` against a test's daemon. */
export class Cli {
  readonly env: NodeJS.ProcessEnv;
  /** When the fixture was set up, just after the daemon started: near enough
   * the start of the test's timeout. */
  private readonly started = Date.now();
  constructor(readonly daemon: Daemon, private readonly testInfo: TestInfo, env: NodeJS.ProcessEnv = {}) {
    this.env = { ...process.env, ...env, PIQUELD_CREDENTIALS_FILE: join(daemon.directory, 'credentials.json') };
    delete this.env.PIQUELD_TOKEN;
  }
  /** Starts `piquelctl` with `args`, in directory `cwd` when given. */
  start(args: string[], token?: string, cwd?: string): Process {
    const env = { ...this.env };
    if (token) env.PIQUELD_TOKEN = token;
    return new Process(join(bin, 'piquelctl'), ['--socket', this.daemon.socket, ...args], env, cwd);
  }
  /** Runs `piquelctl` with `args` to completion, in directory `cwd` and
   * with `input` as its whole standard input when given. A command still
   * running `commandMargin` before the test's timeout, which `test.slow()`
   * scales, is killed and throws with its output so far. */
  async run(args: string[], token?: string, cwd?: string, input?: string) {
    const child = this.start(args, token, cwd);
    if (input !== undefined) child.child.stdin.end(input);
    let killed = false;
    // A timeout of 0 means the test has none.
    const timer = this.testInfo.timeout && setTimeout(() => {
      killed = true;
      child.child.kill('SIGKILL');
    }, this.started + this.testInfo.timeout - commandMargin - Date.now());
    try {
      const code = await child.exited;
      if (killed) throw new Error(`piquelctl ${args.join(' ')} was still running near the test's timeout\nstdout:\n${child.stdout}\nstderr:\n${child.stderr}`);
      return { code, stdout: child.stdout, stderr: child.stderr };
    } finally { clearTimeout(timer); }
  }
}

/** Chromium's real WebAuthn implementation, backed by a virtual CTAP2 device. */
export class Passkeys {
  private constructor(private readonly cdp: CDPSession, private id: string) {}
  static async create(page: Page): Promise<Passkeys> {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('WebAuthn.enable');
    const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', { options: {
      protocol: 'ctap2', transport: 'internal', hasResidentKey: true,
      hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true,
    } });
    return new Passkeys(cdp, authenticatorId);
  }
  async verified(value: boolean) {
    await this.cdp.send('WebAuthn.setUserVerified', { authenticatorId: this.id, isUserVerified: value });
  }
  async reset() {
    await this.cdp.send('WebAuthn.clearCredentials', { authenticatorId: this.id });
  }
  /** The passkeys the authenticator holds, to `restore` after a `reset`. */
  async credentials() {
    return (await this.cdp.send('WebAuthn.getCredentials', { authenticatorId: this.id })).credentials;
  }
  async restore(credentials: Awaited<ReturnType<Passkeys['credentials']>>) {
    for (const credential of credentials) {
      await this.cdp.send('WebAuthn.addCredential', { authenticatorId: this.id, credential });
    }
  }
  async close() { await this.cdp.detach(); }
}

export async function api<T = Record<string, unknown>>(page: Page, path: string, body?: unknown): Promise<{ status: number; body: T }> {
  return page.evaluate(async ({ path, body }) => {
    const response = await fetch(`/api/v1/auth/${path}`, body === undefined ? {} : {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  }, { path, body });
}
export async function auth<T = Record<string, unknown>>(page: Page, path: string, body?: unknown): Promise<T> {
  const result = await api<T>(page, path, body);
  expect(result.status, JSON.stringify(result.body)).toBe(200);
  return result.body;
}
/** A new, never-expiring API token named `name`, with the admin grant, of the signed-in account. */
export async function adminToken(page: Page, name: string): Promise<string> {
  return (await auth<Managed>(page, 'manage', { action: 'create_token', grants: [{ permission: 'admin' }], name, days: null })).token;
}
/** Opens invitation `link` and fills in its account form, the display and
 * passkey names only when given. */
export async function fillRegistration(page: Page, link: string, username: string, names: { display?: string; passkey?: string } = {}) {
  await page.goto(link);
  await page.getByLabel('Username', { exact: true }).fill(username);
  if (names.display) await page.getByLabel('Display name (optional)', { exact: true }).fill(names.display);
  if (names.passkey) await page.getByLabel('Passkey name', { exact: true }).fill(names.passkey);
}
/** Creates the account of the filled-in form, with a passkey, and returns it once signed in. */
export async function submitRegistration(page: Page): Promise<User> {
  await page.getByRole('button', { name: 'Create account with a passkey', exact: true }).click();
  await expect(page).toHaveURL(/\/dashboard\/$/);
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  return (await auth<Session>(page, 'me')).user;
}
export async function register(page: Page, link: string, username: string): Promise<User> {
  await fillRegistration(page, link, username);
  return submitRegistration(page);
}
export async function proof(page: Page, ceremony: Ceremony, registration = false) {
  const credential = await page.evaluate(async ({ options, registration }) => {
    const parser = PublicKeyCredential as unknown as {
      parseCreationOptionsFromJSON(input: unknown): PublicKeyCredentialCreationOptions;
      parseRequestOptionsFromJSON(input: unknown): PublicKeyCredentialRequestOptions;
    };
    const credential = registration
      ? await navigator.credentials.create({ publicKey: parser.parseCreationOptionsFromJSON(options.publicKey) })
      : await navigator.credentials.get({ publicKey: parser.parseRequestOptionsFromJSON(options.publicKey) });
    return (credential as PublicKeyCredential & { toJSON(): { response: Record<string, unknown> } }).toJSON();
  }, { options: ceremony.options, registration });
  return { id: ceremony.id, credential };
}

/** Confirms the signed-in browser session with its passkey, as the
 * dashboard's prompt does, so sensitive actions in the next 2 minutes go
 * through without asking. */
export async function confirmPasskey(page: Page) {
  const ceremony = await auth<Ceremony>(page, 'reauthenticate/start', {});
  await auth(page, 'reauthenticate/finish', await proof(page, ceremony));
}

/** Calls the API from the signed-in page, returning the response's `data`.
 * A string body is sent as raw bytes; a failed response throws. */
export async function call(page: Page, method: string, path: string, body?: unknown, headers: Record<string, string> = {}) {
  return page.evaluate(async ({ method, path, body, headers }) => {
    const raw = typeof body === 'string';
    const response = await fetch(`/api/v1/${path}`, {
      method,
      headers: body === undefined ? headers : { 'Content-Type': raw ? 'application/octet-stream' : 'application/json', ...headers },
      body: body === undefined ? undefined : raw ? body : JSON.stringify(body),
    });
    const value = await response.json();
    if (!response.ok) throw new Error(JSON.stringify(value));
    return value.data;
  }, { method, path, body, headers });
}

/** The ID of application `app`'s environment `name`. */
export async function environmentId(page: Page, app: string, name = 'production'): Promise<string> {
  const { environments } = await call(page, 'GET', `applications/${app}`);
  return environments.find((environment: { name: string }) => environment.name === name).id;
}

/** Saves `manifest` as a new application, returning its ID, its
 * `production` environment's, and its generation. */
export async function apply(page: Page, manifest: Record<string, unknown>): Promise<{ app: string; env: string; generation: number }> {
  const saved = await call(page, 'POST', 'applications/apply', { manifest, expected_generation: 0 });
  const app = saved.application_id as string;
  return { app, env: await environmentId(page, app), generation: saved.generation as number };
}

/** A deployment as the daemon lists it. */
export type Deployment = { operation: Record<string, unknown> } & Record<string, unknown>;

/** `notes`, deployed once, and that deployment as the daemon lists it. The
 * fixture never deploys, so tests derive the deployments they list from it. */
export async function deployed(page: Page) {
  const { app, env, generation } = await apply(page, webManifest('notes'));
  await call(page, 'POST', `environments/${env}/deploy?expected_generation=${generation}`);
  const deployment = (await call(page, 'GET', `environments/${env}/deployments`)).items[0] as Deployment;
  return { app, env, deployment };
}

/** Serves `pages(cursor)` as `environment`'s deployment pages, and each
 * deployment they list as `/deployments/{id}`. */
export async function serveDeployments(page: Page, environment: string, pages: (cursor: string | null) => { items: Deployment[]; next_cursor: string | null }) {
  const list = `/api/v1/environments/${environment}/deployments`;
  await page.route(url => url.pathname === list, async (route: Route) => {
    const cursor = new URL(route.request().url()).searchParams.get('cursor');
    await route.fulfill({ json: { data: pages(cursor) } });
  });
  await page.route(url => /^[^/]+$/.test(url.pathname.slice(list.length + 1)) && url.pathname.startsWith(`${list}/`), async (route: Route) => {
    const id = new URL(route.request().url()).pathname.slice(list.length + 1);
    let cursor: string | null = null;
    do {
      const shown = pages(cursor);
      const found = shown.items.find(item => item.operation.id === id);
      if (found !== undefined) return route.fulfill({ json: { data: found } });
      cursor = shown.next_cursor;
    } while (cursor !== null);
    await route.fulfill({ status: 404, json: { code: 'not_found', message: 'not found', request_id: 'request' } });
  });
}

/** Accepts the next confirmation, once its message contains `text` when given. */
export function confirmNext(page: Page, text?: string) {
  page.once('dialog', async dialog => {
    if (text !== undefined) expect(dialog.message()).toContain(text);
    await dialog.accept();
  });
}

/** Rewrites the JSON `data` of GET responses whose path is exactly `path`. */
export async function rewrite(page: Page, path: string, edit: (data: Record<string, any>) => void) {
  await page.route(url => url.pathname === path, async (route: Route) => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    edit(body.data);
    await route.fulfill({ response, json: body });
  });
}

/** Shows environment `id` resolved at `generation` in application
 * responses: the stubbed runtime never resolves a target. */
export async function resolveEnvironment(page: Page, id: string, generation: number) {
  const resolve = (value: unknown): void => {
    if (Array.isArray(value)) return value.forEach(resolve);
    if (typeof value !== 'object' || value === null) return;
    const record = value as Record<string, unknown>;
    if (record.id === id && 'resolved_generation' in record) record.resolved_generation = generation;
    Object.values(record).forEach(resolve);
  };
  await page.route(url => url.pathname.startsWith('/api/v1/applications'), async (route: Route) => {
    const response = await route.fetch();
    const json = await response.json();
    resolve(json);
    await route.fulfill({ response, json });
  });
}

/** The `spec.manifest` of `notes`, read from `app.toml` on branch `main`
 * of its repository, which the fixture never fetches. */
export const repositoryManifest = { path: 'app.toml', repository: { url: 'https://example.com/notes.git', branch: 'main' } };

/** The manifest of application `name`, running one `web` service from
 * `nginx:alpine`, with the rest of `spec`. */
export const webManifest = (name: string, spec: Record<string, unknown> = {}) => ({
  api_version: 'piqueld.dev/v1alpha1', kind: 'Application',
  metadata: { name },
  spec: { services: [{ name: 'web', source: { type: 'image', image: 'nginx:alpine' } }], ...spec },
});

/** One of the application page's section tabs. */
export const applicationTab = (page: Page, name: string) =>
  page.getByRole('tablist', { name: 'Application sections' }).getByRole('tab', { name, exact: true });
/** One of an environment or preview page's section tabs. */
export const environmentTab = (page: Page, name: string) =>
  page.getByRole('tablist', { name: 'Environment sections' }).getByRole('tab', { name, exact: true });
/** One of a service page's section tabs. */
export const serviceTab = (page: Page, name: string) =>
  page.getByRole('tablist', { name: 'Service sections' }).getByRole('tab', { name, exact: true });
/** The card of a page section, by its heading. */
export const card = (page: Page, heading: string) =>
  page.locator('section.card', { has: page.getByRole('heading', { name: heading, exact: true }) });
/** The title of an application, environment, or service page. */
export const title = (page: Page) => page.locator('.detail-title h1');

/** The rows of the Deployments tab, newest first. */
export const deploymentRows = (page: Page) => page.getByRole('list', { name: 'Deployments', exact: true }).getByRole('listitem');

/** Opens the page of the deployment a row of the Deployments tab shows. */
export async function openDeployment(page: Page, row: Locator) {
  await row.getByRole('link').first().click();
  await expect(page).toHaveURL(/\/deployments\/[^/?]+/);
}

/** One of a deployment page's section tabs. */
export const deploymentTab = (page: Page, name: string) =>
  page.getByRole('tablist', { name: 'Deployment sections' }).getByRole('tab', { name, exact: true });
/** The header of an application, environment, or service page, with its actions. */
export const head = (page: Page) => page.locator('.detail-head');

/** The rows of the Accounts page's directory. */
export const accountRows = (page: Page) => page.getByRole('list', { name: 'Accounts' }).getByRole('listitem');

/** Opens account `username`'s page from the Accounts page, on `tab` when
 * given, and returns the page's main region. */
export async function openAccount(page: Page, username: string, tab?: 'Profile' | 'Access'): Promise<Locator> {
  await page.goto('/dashboard/accounts');
  await accountRows(page).getByRole('link', { name: username, exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: username, exact: true })).toBeVisible();
  if (tab) await page.getByRole('tablist', { name: 'Account sections' }).getByRole('tab', { name: tab, exact: true }).click();
  return page.getByRole('main');
}

/** Chooses grants in the grant editor within `scope`, as the guides do: the
 * applications first, if any, then the preset. */
export async function chooseGrants(scope: Locator, preset: string, applications: string[] = []) {
  if (applications.length) {
    await scope.getByLabel('Application permissions apply to every application, including future ones').setChecked(false);
    for (const application of applications) await scope.getByLabel(application, { exact: true }).setChecked(true);
  }
  await scope.getByLabel('Preset').selectOption(preset);
}

/** The invitation link, enrollment link or API token the Accounts page shows
 * once, read before selecting **Done**. */
export async function done(page: Page): Promise<string> {
  const secret = page.locator('.secret-box');
  await expect(secret).toContainText(/#(invite|enroll)=|pqd_/);
  const link = (await secret.innerText()).trim();
  await page.getByRole('button', { name: 'Done', exact: true }).click();
  await expect(secret).toHaveCount(0);
  return link;
}

/** Creates account `username` through invitation `link`, in a browser of its own,
 * with display name `display` when given. */
export async function signUp(browser: Browser, link: string, username: string, display?: string) {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const passkeys = await Passkeys.create(page);
    try {
      await fillRegistration(page, link, username, { display, passkey: 'Laptop' });
      await submitRegistration(page);
    } finally { await passkeys.close(); }
  } finally { await context.close(); }
}

/** Fixture options a configuration can set under `use`. */
export type Options = {
  /** Deploy to the fixture's in-memory Docker engine, so deployments succeed,
   * instead of never deploying. */
  fakeDocker: boolean;
  /** Variables added to the environment of the daemon and of `piquelctl`. */
  processEnv: NodeJS.ProcessEnv;
};

export const test = base.extend<Options & { daemon: Daemon; passkeys: Passkeys; account: User; cli: Cli; browserErrors: void }>({
  fakeDocker: [false, { option: true }],
  processEnv: [{}, { option: true }],
  daemon: async ({ fakeDocker, processEnv }, use, testInfo) => {
    const directory = await mkdtemp(join(tmpdir(), 'piqueld-playwright-'));
    const env: NodeJS.ProcessEnv = { ...process.env, ...processEnv, PIQUELD_PLAYWRIGHT_DATA_DIR: directory };
    if (fakeDocker) env.PIQUELD_PLAYWRIGHT_FAKE_DOCKER = '1';
    const child = new Process(join(bin, 'examples/browser_fixture'), [], env);
    try {
      const line = await child.waitForOutput(/^(\{[^\n]+\})/m, 'stdout');
      const daemon: Daemon = { ...JSON.parse(line), directory };
      await use(daemon);
    } finally {
      try {
        await child.stop();
        await testInfo.attach('daemon.log', { body: child.stderr, contentType: 'text/plain' });
      } finally { await rm(directory, { recursive: true, force: true }); }
    }
  },
  baseURL: async ({ daemon }, use) => { await use(daemon.origin); },
  // The dashboard keeps fetching until the context closes, which disposes the
  // response of any request a test's route handler is still rewriting.
  page: async ({ page }, use) => {
    await use(page);
    await page.unrouteAll({ behavior: 'ignoreErrors' });
  },
  passkeys: async ({ page }, use) => {
    const passkeys = await Passkeys.create(page);
    try { await use(passkeys); } finally { await passkeys.close(); }
  },
  // Confirmed with its passkey, as a setup link's session isn't, so the
  // test may create credentials and links in its first 2 minutes.
  account: async ({ page, daemon, passkeys }, use) => {
    void passkeys;
    const account = await register(page, daemon.setup, 'alice');
    await confirmPasskey(page);
    await use(account);
  },
  cli: async ({ daemon, processEnv }, use, testInfo) => { await use(new Cli(daemon, testInfo, processEnv)); },
  browserErrors: [async ({ page }, use) => {
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await use();
    expect(errors, 'Uncaught browser errors').toEqual([]);
  }, { auto: true }],
});
export { expect };
