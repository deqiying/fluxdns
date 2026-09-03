import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { AppErrorBoundary } from "@/app/AppErrorBoundary";
import "antd/dist/reset.css";
import "@/styles/index.css";

async function bootstrap() {
  if (import.meta.env.DEV && import.meta.env.VITE_USE_MOCK_API === "true") {
    const { worker } = await import("@/mocks/browser");
    await worker.start({ onUnhandledRequest: "bypass" });
  }

  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <AppErrorBoundary>
        <AppProviders>
          <App />
        </AppProviders>
      </AppErrorBoundary>
    </StrictMode>,
  );
}

void bootstrap();
