import type { Locator } from '@playwright/test';
import { test, expect, confirmNext } from '../fixtures.js';

test('daemon settings change in their fields, say why a value is refused, and reset', async ({ page, account }) => {
  void account;
  await page.goto('/dashboard/settings');
  const row = (key: string) => page.getByRole('listitem').filter({ hasText: key });
  /** How far `below` is under `above`, which a change must not alter. */
  const gap = async (above: Locator, below: Locator) => (await below.boundingBox())!.y - (await above.boundingBox())!.y;
  const events = row('retention.event_days');
  const days = page.getByRole('textbox', { name: 'Application events (days)', exact: true });
  const reset = events.getByRole('button', { name: 'Reset Application events (days) to its default, 90', exact: true });
  await expect(days).toHaveValue('90');
  await expect(reset).toBeHidden();

  // Leaving the field saves nothing, and Cancel brings the value back.
  const next = page.getByRole('textbox', { name: 'Daemon events (days)', exact: true });
  const apart = await gap(days, next);
  await days.fill('3');
  await days.blur();
  await expect(events).toContainText('Not saved');
  await expect(reset).toBeHidden();
  // Pressed with the keyboard, Cancel gives the focus back to the field.
  await events.getByRole('button', { name: 'Cancel', exact: true }).press('Enter');
  await expect(days).toHaveValue('90');
  await expect(days).toBeFocused();

  // Keeping less history asks first: declined, the number stays in its
  // field, not saved.
  await days.fill('30');
  page.once('dialog', dialog => dialog.dismiss());
  await days.press('Enter');
  await expect(events).toContainText('Not saved');
  await expect(reset).toBeHidden();

  // Confirmed, Enter saves; the field stays where it is, and its note
  // says so.
  confirmNext(page, 'Change Application events (days) from 90 to 30? That keeps less history');
  await days.press('Enter');
  await expect(events).toContainText('Saved');
  await expect(events).toContainText('Default: 90');
  await expect(reset).toBeVisible();
  expect(await gap(days, next)).toBe(apart);

  // A refused value says why in its note, which moves nothing; Escape
  // brings the setting's value back.
  const scan = row('reconciliation.scan_interval_seconds');
  const seconds = page.getByRole('textbox', { name: 'Scan interval (seconds)', exact: true });
  const below = page.getByRole('textbox', { name: 'Preparation timeout (seconds)', exact: true });
  const under = await gap(seconds, below);
  await seconds.fill('0');
  await seconds.press('Enter');
  await expect(scan).toContainText('Enter a whole number from 1 to 86400.');
  await expect(seconds).toHaveValue('0');
  await expect(seconds).toHaveAttribute('aria-invalid', 'true');
  expect(await gap(seconds, below)).toBe(under);
  await seconds.press('Escape');
  await expect(seconds).toHaveValue('60');
  await expect(scan).not.toContainText('a whole number');
  // So does Cancel, which stays beside the refusal.
  await seconds.fill('0');
  await seconds.press('Enter');
  await scan.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(seconds).toHaveValue('60');

  // A size takes bytes alone, and its refusal, without the maximum
  // nobody means to reach, fits its line too.
  const memory = page.getByRole('textbox', { name: 'Default memory (bytes)', exact: true });
  const replicas = page.getByRole('textbox', { name: 'Replicas per service', exact: true });
  const beneath = await gap(memory, replicas);
  await memory.fill('512M');
  await memory.press('Enter');
  await expect(row('previews.default_memory_bytes')).toContainText('Enter a whole number of at least 1.');
  expect(await gap(memory, replicas)).toBe(beneath);
  await memory.press('Escape');

  // Save in the note saves too, as on a phone's keypad without Enter.
  await seconds.fill('120');
  await scan.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(scan).toContainText('Default: 60');

  // While a number isn't saved, Tab goes from the field to Save, past the
  // reset button.
  await days.fill('31');
  await page.keyboard.press('Tab');
  await expect(events.getByRole('button', { name: 'Save', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(events).toContainText('Saved');
  await expect(days).toBeFocused();

  // Reset says what the value was, and leaves the focus in the field.
  await reset.press('Enter');
  await expect(events).toContainText('Reset, was 31');
  await expect(days).toHaveValue('90');
  await expect(days).toBeFocused();
  await expect(reset).toBeHidden();

  // A reset that keeps less history asks first too.
  const audit = page.getByRole('textbox', { name: 'Audit trail (days)', exact: true });
  await audit.fill('400');
  await audit.press('Enter');
  await expect(row('retention.audit_days')).toContainText('Default: 365');
  confirmNext(page, 'Change Audit trail (days) from 400 to 365?');
  await page.getByRole('button', { name: 'Reset Audit trail (days) to its default, 365', exact: true }).click();
  await expect(audit).toHaveValue('365');

  // Keeping fewer images asks too, saying what it deletes.
  const images = page.getByRole('textbox', { name: 'Deployments kept per environment', exact: true });
  await images.fill('1');
  confirmNext(page, 'Change Deployments kept per environment from 3 to 1? That keeps fewer images');
  await images.press('Enter');
  await expect(row('images.keep_deployments')).toContainText('Default: 3');

  // A switch saves when toggled, by its name too.
  const traffic = page.getByRole('switch', { name: 'Count traffic', exact: true });
  await row('ingress.traffic').getByText('Count traffic', { exact: true }).click();
  await expect(traffic).not.toBeChecked();
  await expect(row('ingress.traffic')).toContainText('Default: On');
  await page.getByRole('button', { name: 'Reset Count traffic to its default, On', exact: true }).click();
  await expect(traffic).toBeChecked();

  // A setting with no limit takes an empty field.
  const lifetime = page.getByRole('textbox', { name: 'Longest token lifetime (days)', exact: true });
  await expect(lifetime).toHaveValue('');
  await lifetime.fill('30');
  await lifetime.press('Enter');
  await expect(row('auth.max_token_days')).toContainText('Default: No limit');

  // Without a build machine, its fallback has no field.
  const unavailable = row('build.unavailable');
  await expect(unavailable).toContainText('Applies only with a build machine');
  await expect(unavailable.getByRole('combobox')).toHaveCount(0);

  // The host's settings follow, read-only.
  await expect(page.getByRole('heading', { name: 'Server', exact: true })).toBeVisible();
});
