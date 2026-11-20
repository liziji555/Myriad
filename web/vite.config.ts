import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const backend = process.env.SA_BACKEND ?? "http://127.0.0.1:4517";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: { "/api": backend, "/auth": backend },
  },
  test: {
    include: ["src/**/*.test.ts"],
  },
});
