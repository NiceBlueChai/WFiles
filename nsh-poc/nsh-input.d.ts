// `Input` 的类型补丁（0.4.0）。
//
// 为什么需要：0.4.0 把 `Input` 从 `nsh` 搬到 `nsh/base`，但**生成出来的
// `nsh/base.d.ts` 里没有 `Input` 这个成员**——它在 `nsh-js` 的 `ELEMENTS`
// 里只是一条 `Registry::native_values` 的运行时注册
// （`crates/nsh-js/src/env.rs` 的「declared by hand」），`declarations.rs`
// 负责写 d.ts 的那一遍没有把它吐出来，于是只剩 `InputHostProps`（在
// `nsh/types.d.ts` 里，`Input` 的属性确实齐）。
//
// 框架自己的夹具就是照 `nsh/base` import 的
// （`crates/nsh-tui-host/tests/fixtures/text-input/App.tsx`），但夹具不做
// 类型检查，所以上游没暴露这个缺口。
//
// 上游补上之后删掉本文件即可：`nsh check` 若仍然绿，说明补丁可以退休。
import type { InputHostProps } from "nsh";

declare module "nsh/base" {
  /** 原生文本输入（UITextField / UITextView / EditText）。 */
  export const Input: (
    props: InputHostProps & {
      class?: string;
      style?: unknown;
      testID?: string;
      /** 无障碍标签（判据按 `label:` 找它）。 */
      accessibilityLabel?: string;
    },
  ) => import("nsh").JSX.Element;
}
