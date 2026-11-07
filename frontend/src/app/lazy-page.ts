import { lazy, type ComponentType } from "react";

/** 页面模块加载或求值失败；保留原始原因供诊断，不向页面展示资源 URL 或错误正文。 */
export class PageLoadError extends Error {
  constructor(cause: unknown) {
    super("页面模块加载失败", { cause });
    this.name = "PageLoadError";
  }
}

/**
 * 为无 props 的路由入口统一标记加载失败，不依赖浏览器各异的 import/CSS 错误文案。
 * 只包裹加载阶段：模块加载成功后的渲染异常仍由普通错误边界处理。
 * 不重试已被浏览器或 React 缓存的失败模块，也不自动刷新并丢弃未保存修改。
 */
export function lazyPage(load: () => Promise<{ default: ComponentType }>) {
  return lazy(async () => {
    try {
      return await load();
    } catch (cause) {
      throw new PageLoadError(cause);
    }
  });
}
