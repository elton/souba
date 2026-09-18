import { cloudflareTest, readD1Migrations } from '@cloudflare/vitest-pool-workers'
import { defineConfig } from 'vitest/config'

// 测试跑的是真正的 migrations/ 目录 —— schema 漂了测试就会红
const migrations = await readD1Migrations('./migrations')

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: './wrangler.jsonc' },
      miniflare: {
        // vitest-pool-workers 0.22.0 内置的 workerd 最高只认到 2026-08-22，
        // 而线上用的是 wrangler.jsonc 里的 2026-08-26。只在测试里压低这一档。
        compatibilityDate: '2026-08-22',
        bindings: { SOUBA_SYNC_KEY: 'test-key', TEST_MIGRATIONS: migrations },
      },
    }),
  ],
  test: {
    setupFiles: ['./test/apply-migrations.ts'],
  },
})
