import { test, expect, rewrite } from '../fixtures.js';

test('the builds page shows the build machine, and warns while it is unreachable', async ({ page, account }) => {
  void account;
  // The fixture daemon has no build machine: its status and a check report one.
  const machine = { endpoint: 'tcp://build.example.ts.net:1234', repository: 'build.example.ts.net:5000/piqueld' };
  await rewrite(page, '/api/v1/system/status', data => {
    data.builder = {
      ...machine,
      last_check: { reachable: false, checked_at_ms: Date.now() - 60_000, message: 'Connection refused (os error 111)' },
    };
  });
  await page.route(url => url.pathname === '/api/v1/system/builder/check', route =>
    route.fulfill({ json: { data: { reachable: true, checked_at_ms: Date.now() } } }));
  await page.goto('/dashboard/builds');
  const builder = page.getByRole('group', { name: 'Build machine' });
  await expect(builder).toContainText('Images build on the build machine at tcp://build.example.ts.net:1234');
  await expect(builder).toContainText('It was unreachable');
  await expect(builder).toContainText('Connection refused');
  await builder.getByRole('button', { name: 'Check now', exact: true }).click();
  await expect(builder).toContainText('It answered');
});

/** A build listed by the daemon, as `/api/v1/builds` returns it. */
const build = (id: number, state: string) => ({
  id, environment_id: 'app-shop0001', operation_id: `operation-${id}`, service: 'web',
  source: { type: 'image', image: 'nginx:alpine' }, state,
  started_at_ms: Date.now(), finished_at_ms: null, log_bytes: 9, log_truncated: false, log_expired: false,
});

test('an idle builds page shows a build started elsewhere', async ({ page, account }) => {
  void account;
  const builds: unknown[] = [];
  await page.route(url => url.pathname === '/api/v1/builds', route =>
    route.fulfill({ json: { data: { items: builds, next_cursor: null } } }));
  await page.clock.install();
  await page.goto('/dashboard/builds');
  await expect(page.getByText('No builds recorded yet.')).toBeVisible();
  builds.push(build(1, 'succeeded'));
  await page.clock.runFor(16_000);
  await expect(page.getByRole('button', { name: /Build #1/ })).toBeVisible();
});

test('a running build\'s periodic output fetch keeps older output loaded', async ({ page, account }) => {
  void account;
  await page.route(url => url.pathname === '/api/v1/builds', route =>
    route.fulfill({ json: { data: { items: [build(1, 'running')], next_cursor: null } } }));
  let newest = 'newest output\n';
  await page.route(url => url.pathname === '/api/v1/builds/1/logs', route => {
    const older = new URL(route.request().url()).searchParams.get('before') === '10';
    const items = older
      ? [{ offset: 0, timestamp_ms: 1, stream: 'stdout', text: 'older output\n' }]
      : [{ offset: 10, timestamp_ms: 2, stream: 'stdout', text: newest }];
    return route.fulfill({ json: { data: { items, previous_offset: older ? null : 10, truncated: false, expired: false } } });
  });
  await page.clock.install();
  await page.goto('/dashboard/builds');
  await page.getByRole('button', { name: /Build #1/ }).click();
  const output = page.getByRole('region', { name: 'Build log output' });
  await expect(output).toContainText('newest output');
  await page.getByRole('button', { name: 'Load older output', exact: true }).click();
  await expect(output).toContainText('older output');

  newest = 'newest output, continued\n';
  const fetched = page.waitForRequest(request => new URL(request.url()).pathname === '/api/v1/builds/1/logs');
  await page.clock.runFor(31_000);
  await fetched;
  await expect(output).toContainText('newest output, continued');
  await expect(output).toContainText('older output');
  await expect(page.getByRole('button', { name: 'Load older output', exact: true })).toBeHidden();
});
