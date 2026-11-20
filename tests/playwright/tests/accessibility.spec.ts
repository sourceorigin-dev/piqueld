import { AxeBuilder } from '@axe-core/playwright';
import { test, expect, apply, webManifest } from '../fixtures.js';
import type { Page } from '@playwright/test';

/** How severe a violation must be to fail: only serious and critical ones
 * for now, while the moderate and minor ones are worked through. */
const failing = new Set(['serious', 'critical']);

/** Scans the shown page with axe and expects no failing violation, naming
 * each one's rule and elements so a failure says what to fix. */
async function scan(page: Page, label: string) {
  const { violations } = await new AxeBuilder({ page }).analyze();
  const found = violations
    .filter(violation => failing.has(violation.impact ?? ''))
    .map(violation => `${violation.id} (${violation.impact}): ${violation.nodes.map(node => node.target.join(' ')).join(', ')}`);
  expect(found, `accessibility violations on ${label}`).toEqual([]);
}

/** Scans `path`, then each section tab it has, once its panel is shown. */
async function scanPage(page: Page, path: string) {
  await page.goto(path);
  await expect(page.locator('main h1').first()).toBeVisible();
  const tabs = page.getByRole('tablist').first().getByRole('tab');
  const count = await tabs.count();
  if (count === 0) return scan(page, path);
  for (let index = 0; index < count; index += 1) {
    const tab = tabs.nth(index);
    await tab.click();
    await expect(tab).toHaveAttribute('aria-selected', 'true');
    await scan(page, `${path}, ${await tab.textContent()} tab`);
  }
}

test('the main dashboard pages have no serious accessibility violations', async ({ page, account }) => {
  const { app, env } = await apply(page, webManifest('shop'));
  for (const path of [
    '/dashboard/',
    '/dashboard/applications',
    `/dashboard/applications/${app}`,
    `/dashboard/applications/${app}/environments/${env}`,
    '/dashboard/builds',
    '/dashboard/accounts',
    `/dashboard/accounts/${account.id}`,
    '/dashboard/system',
  ]) {
    await scanPage(page, path);
  }
});
