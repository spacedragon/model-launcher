import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const { bootstrap } = vi.hoisted(() => ({ bootstrap: vi.fn() }));
vi.mock("./bridge", () => ({
  bridge: {
    bootstrap,
    listen: vi.fn().mockResolvedValue(() => undefined),
    logs: vi.fn().mockResolvedValue([]),
    rescan: vi.fn(), load: vi.fn(), eject: vi.fn(), exportLogs: vi.fn(),
    saveEngine: vi.fn(), saveServer: vi.fn(), generateToken: vi.fn(),
    minimize: vi.fn(), maximize: vi.fn(), close: vi.fn(),
  },
}));

const fixture = {
  models: [
    { id:"1", key:"qwen", name:"Qwen 8B", path:"D:\\models\\qwen.gguf", fileName:"qwen.gguf", sizeBytes:4, size:"4 GB", state:"ready", running:true, settings:{} },
    { id:"2", key:"llama", name:"Llama 3B", path:"D:\\models\\llama.gguf", fileName:"llama.gguf", sizeBytes:2, size:"2 GB", state:"missing", running:false, settings:{} },
  ],
  recentModels: [], lifecycle:{state:"running",desiredModel:"1",inFlight:0}, capabilities:{context_length:true},
  authenticationStatus:"Bearer Token 已启用",serverWarning:"",engineValid:true,
  engineSettings:{distribution:"Ubuntu",executable:"/usr/bin/llama-server",model_directory:"D:\\models",defaults:{}},
  serverSettings:{bind_address:"127.0.0.1",port:1234,auth_enabled:true},baseUrl:"http://127.0.0.1:1234",
};

describe("Model Launcher", () => {
  beforeEach(() => bootstrap.mockResolvedValue(fixture));
  afterEach(cleanup);

  it("filters the model list", async () => {
    render(<App />);
    await screen.findAllByText("Qwen 8B");
    fireEvent.change(screen.getByPlaceholderText("搜索模型名称、API 名称或路径"), { target:{value:"llama"} });
    expect(screen.queryByText("qwen.gguf")).not.toBeInTheDocument();
    expect(screen.getByText("Llama 3B")).toBeInTheDocument();
  });

  it("navigates to API service", async () => {
    render(<App />);
    await screen.findAllByText("Qwen 8B");
    fireEvent.click(screen.getByRole("button", { name:/API 服务/ }));
    await waitFor(() => expect(screen.getByText("OpenAI 兼容接口")).toBeInTheDocument());
  });
});
