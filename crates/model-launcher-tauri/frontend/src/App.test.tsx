import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const { bootstrap, load, eject, logs, listeners } = vi.hoisted(() => ({
  bootstrap: vi.fn(), load: vi.fn(), eject: vi.fn(), logs: vi.fn(),
  listeners: new Map<string, (payload: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  bridge: {
    bootstrap,
    estimateContext: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn(async (event: string, callback: (payload: unknown) => void) => {
      listeners.set(event, callback);
      return () => listeners.delete(event);
    }),
    logs,
    rescan: vi.fn(), load, eject, exportLogs: vi.fn(),
    saveEngine: vi.fn(), saveServer: vi.fn(), generateToken: vi.fn(),
    minimize: vi.fn(), maximize: vi.fn(), close: vi.fn(),
  },
}));

const fixture = {
  models: [
    { id:"1", key:"qwen", name:"Qwen 8B", path:"D:\\models\\qwen.gguf", fileName:"qwen.gguf", sizeBytes:4, size:"4 GB", state:"ready", running:true, settings:{}, metadata:{architecture:"qwen",context_length:32768} },
    { id:"2", key:"llama", name:"Llama 3B", path:"D:\\models\\llama.gguf", fileName:"llama.gguf", sizeBytes:2, size:"2 GB", state:"ready", running:false, settings:{}, metadata:{architecture:"llama",context_length:8192} },
  ],
  recentModels: [], lifecycle:{state:"running",desiredModel:"1",inFlight:0}, capabilities:{context_length:true,speculative_type:true,draft_model:true},
  authenticationStatus:"Bearer Token 已启用",serverWarning:"",engineValid:true,
  engineSettings:{distribution:"Ubuntu",executable:"/usr/bin/llama-server",model_directory:"D:\\models",defaults:{}},
  serverSettings:{bind_address:"127.0.0.1",port:1234,auth_enabled:true},baseUrl:"http://127.0.0.1:1234",
  gpuMemory:{name:"Test GPU",total_bytes:16 * 1024 ** 3,free_bytes:12 * 1024 ** 3},
};

describe("Model Launcher", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    listeners.clear();
    bootstrap.mockResolvedValue(fixture);
    load.mockResolvedValue(undefined);
    eject.mockResolvedValue(undefined);
    logs.mockResolvedValue([]);
  });
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

  it("opens model parameters before loading", async () => {
    render(<App />);
    await screen.findByText("Llama 3B");
    const row = screen.getByText("Llama 3B").closest("article");
    fireEvent.click(row!.querySelector("button")!);
    expect((await screen.findAllByText("8,192 tokens")).length).toBeGreaterThan(0);
    expect(load).not.toHaveBeenCalled();
  });

  it("shows live logs and can close after a successful load", async () => {
    logs.mockResolvedValue([{ timestamp_ms:Date.now(), source:"engine_stderr", level:"info", model_id:"2", message:"loading tensors" }]);
    render(<App />);
    await screen.findByText("Llama 3B");
    fireEvent.click(screen.getByText("Llama 3B").closest("article")!.querySelector("button")!);
    fireEvent.click(await screen.findByRole("button", { name:"保存并加载" }));

    const dialog = await screen.findByRole("dialog", { name:"加载 Llama 3B" });
    expect(await screen.findByText("loading tensors")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name:"关闭" })).not.toBeInTheDocument();
    expect(load).toHaveBeenCalledWith("2", "llama", expect.any(Object));

    act(() => listeners.get("load-finished")?.({ modelId:"2", success:true, message:"模型已加载并可以使用。" }));
    expect(await screen.findByText("加载完成")).toBeInTheDocument();
    fireEvent.click(within(dialog).getByRole("button", { name:"关闭" }));
    expect(screen.queryByRole("dialog", { name:"加载 Llama 3B" })).not.toBeInTheDocument();
  });

  it("keeps the dialog open while cancellation safely finishes", async () => {
    render(<App />);
    await screen.findByText("Llama 3B");
    fireEvent.click(screen.getByText("Llama 3B").closest("article")!.querySelector("button")!);
    fireEvent.click(await screen.findByRole("button", { name:"保存并加载" }));
    fireEvent.click(await screen.findByRole("button", { name:"取消加载" }));

    expect(eject).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name:"正在取消…" })).toBeDisabled();
    const dialog = screen.getByRole("dialog", { name:"加载 Llama 3B" });
    expect(within(dialog).queryByRole("button", { name:"关闭" })).not.toBeInTheDocument();

    act(() => listeners.get("load-finished")?.({ modelId:"2", success:false, message:"load cancelled" }));
    expect(await screen.findByText("加载已取消，启动中的进程已安全停止。")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name:"关闭" })).toBeInTheDocument();
  });

  it("requires and submits a separate DFlash 2 draft model", async () => {
    render(<App />);
    await screen.findByText("Llama 3B");
    const row = screen.getByText("Llama 3B").closest("article");
    fireEvent.click(row!.querySelector("button")!);

    fireEvent.change(await screen.findByLabelText("投机解码"), { target:{value:"draft-dflash"} });
    const save = screen.getByRole("button", { name:"保存并加载" });
    expect(screen.getByLabelText("DFlash 2 草稿模型")).toBeInTheDocument();
    expect(save).toBeDisabled();

    fireEvent.change(screen.getByLabelText("DFlash 2 草稿模型"), { target:{value:"D:\\models\\qwen.gguf"} });
    fireEvent.click(save);

    await waitFor(() => expect(load).toHaveBeenCalledWith("2", "llama", expect.objectContaining({
      speculative_type:"draft-dflash",
      draft_model:"D:\\models\\qwen.gguf",
    })));
  });
});
