export interface Route {
  path: string;
  segments: string[];
}

export interface AppRouter {
  current(): Route;
  navigate(path: string): Promise<void>;
  subscribe(listener: (route: Route) => Promise<void>): void;
}

export function createRouter(window: Window): AppRouter {
  let listener: ((route: Route) => Promise<void>) | undefined;
  const route = (): Route => ({
    path: window.location.pathname,
    segments: window.location.pathname.split("/").filter(Boolean),
  });
  const render = async () => listener?.(route());
  window.addEventListener("popstate", () => void render());
  return {
    current: route,
    async navigate(path: string): Promise<void> {
      if (window.location.pathname !== path) window.history.pushState({}, "", path);
      await render();
    },
    subscribe(next): void {
      listener = next;
    },
  };
}
