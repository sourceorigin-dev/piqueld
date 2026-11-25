import { test, expect, confirmNext } from '../fixtures.js';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('repositories are created with a recovery key shown once, checked and removed', async ({ page, account }) => {
  void account;
  const parent = await mkdtemp(join(tmpdir(), 'piqueld-backups-'));
  const directory = join(parent, 'repository');
  try {
    await page.goto('/dashboard/backups');
    await expect(page.getByText('No backup repositories.', { exact: false })).toBeVisible();
    await page.getByRole('button', { name: 'Create repository', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Create backup repository' });
    await dialog.getByRole('textbox', { name: 'Name', exact: true }).fill('local');
    // A relative path is refused in the dialog, before anything is sent.
    await dialog.getByRole('textbox', { name: "Directory on the daemon's host" }).fill('backups');
    await dialog.getByRole('button', { name: 'Create repository', exact: true }).click();
    await expect(dialog.getByRole('alert')).toContainText('must be absolute');
    await dialog.getByRole('textbox', { name: "Directory on the daemon's host" }).fill(directory);
    await dialog.getByRole('button', { name: 'Create repository', exact: true }).click();
    await expect(dialog).toBeHidden();

    const kit = page.getByRole('heading', { name: 'Save the recovery key of local' });
    await expect(kit).toBeVisible();
    await expect(page.locator('.secret-box')).toHaveText(/^[0-9a-f]{64}$/);
    expect((await stat(join(directory, 'config'))).isFile()).toBe(true);
    await page.getByRole('button', { name: 'Done', exact: true }).click();
    await expect(kit).toBeHidden();

    const row = page.getByRole('row', { name: /local/ });
    await expect(row).toContainText(directory);
    await expect(row).toContainText('never');
    await row.getByRole('button', { name: 'Check', exact: true }).click();
    await expect(page.getByText('local passed its check.')).toBeVisible();
    await expect(row).toContainText('passed');

    confirmNext(page);
    await row.getByRole('button', { name: 'Remove', exact: true }).click();
    await expect(page.getByText('Repository removed; its backups were kept.')).toBeVisible();
    expect((await stat(join(directory, 'config'))).isFile()).toBe(true);
  } finally {
    await rm(parent, { recursive: true, force: true });
  }
});
