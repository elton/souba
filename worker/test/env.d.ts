import type { D1Migration } from '@cloudflare/vitest-pool-workers'
import type { Env } from '../src/index'

// `cloudflare:test` 的 env 类型取自全局的 Cloudflare.Env，
// 测试里再合并进只有测试才有的迁移绑定。
declare global {
  namespace Cloudflare {
    interface Env extends SoubaEnv {
      /** vitest.config.ts 把 migrations/ 读进来，setupFiles 里跑一遍 */
      TEST_MIGRATIONS: D1Migration[]
    }
  }
}

type SoubaEnv = Env
