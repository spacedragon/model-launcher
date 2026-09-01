import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const { bootstrap, load } = vi.hoisted(() => ({ bootstrap: vi.fn(), load: vi.fn() }));
vi.mock("./bridge", () => ({
  bridge: {
    bootstrap,
    chatCompletion: vi.fn(),
    estimateContext: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockResolvedValue(() => undefined),
    logs: vi.fn().mockResolvedValue([]),
    rescan: vi.fn(), load, eject: vi.fn(), exportLogs: vi.fn(),
    saveEngine: vi.fn(), saveServer: vi.fn(), generateToken: vi.fn(),
    minimize: vi.fn(), maximize: vi.fn(), close: vi.fn(),
  },
}));

const fixture = {
  models: [
    { id:"1", key:"qwen", name:"Qwen 8B", path:"D:\\models\\qwen.gguf", fileName:"qwen.gguf", sizeBytes:4, size:"4 GB", state:"ready", running:true, settings:{}, metadata:{architecture:"qwen",context_length:32768} },
    { id:"2", key:"llama", name:"Llama 3B", path:"D:\\models\\llama.gguf", fileName:"llama.gguf", sizeBytes:2, size:"2 GB", state:"ready", running:false, settings:{}, metadata:{architecture:"llama",context_length:8192} },
  ],
  recentModels: [], lifecycle:{state:"running",desiredModel:"1",inFlight:0}, capabilities:{context_length:true},
  authenticationStatus:"Bearer Token 已启用",serverWarning:"",engineValid:true,
  engineSettings:{distribution:"Ubuntu",executable:"/usr/bin/llama-server",model_directory:"D:\\models",defaults:{}},
  serverSettings:{bind_address:"127.0.0.1",port:1234,auth_enabled:true},baseUrl:"http://127.0.0.1:1234",
  gpuMemory:{name:"Test GPU",total_bytes:16 * 1024 ** 3,free_bytes:12 * 1024 ** 3},
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

  it("exposes chat navigation for the running model", async () => {
    render(<App />);
    await screen.findAllByText("Qwen 8B");
    fireEvent.click(screen.getByRole("button", { name: /Chat/ }));
    expect(await screen.findByText("Start a local conversation")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Message qwen")).toBeInTheDocument();
  });

  it("opens model parameters before loading", async () => {
    render(<App />);
    await screen.findByText("Llama 3B");
    const row = screen.getByText("Llama 3B").closest("article");
    fireEvent.click(row!.querySelector("button")!);
    expect((await screen.findAllByText("8,192 tokens")).length).toBeGreaterThan(0);
    expect(load).not.toHaveBeenCalled();
  });
});
