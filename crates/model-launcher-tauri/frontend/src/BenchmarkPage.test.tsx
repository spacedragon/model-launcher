import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BenchmarkPage } from "./BenchmarkPage";
import type { Model } from "./types";

const { runBenchmark, cancelBenchmark } = vi.hoisted(() => ({
  runBenchmark: vi.fn(),
  cancelBenchmark: vi.fn(),
}));
vi.mock("./bridge", () => ({ bridge: { runBenchmark, cancelBenchmark } }));

const model: Model = {
  id: "1", key: "qwen", name: "Qwen", path: "qwen.gguf", fileName: "qwen.gguf",
  sizeBytes: 4, size: "4 GB", state: "ready", running: true, settings: {}, metadata: {},
};

describe("BenchmarkPage", () => {
  beforeEach(() => {
    runBenchmark.mockReset().mockResolvedValue({
      latencyMs: 120, ttftMs: 45, completionTokens: 12, tokensPerSecond: 20, tokenCountEstimated: false,
    });
    cancelBenchmark.mockReset().mockResolvedValue(true);
  });
  afterEach(cleanup);

  it("runs the configured sample and supports re-running it", async () => {
    render(<BenchmarkPage baseUrl="http://127.0.0.1:1234" model={model} authEnabled={false} />);
    fireEvent.change(screen.getByLabelText("运行次数"), { target: { value: "2" } });
    fireEvent.click(screen.getByRole("button", { name: "开始基准测试" }));

    await screen.findByText("2/2 次完成");
    expect(runBenchmark).toHaveBeenCalledTimes(2);
    expect(runBenchmark).toHaveBeenCalledWith(expect.objectContaining({ model: "qwen", maxTokens: 64 }));
    expect(screen.getAllByText("20.0 tok/s")).toHaveLength(3);

    fireEvent.click(screen.getByRole("button", { name: "重新运行" }));
    await waitFor(() => expect(runBenchmark).toHaveBeenCalledTimes(4));
  });

  it("cancels the active request and retains cancellation state", async () => {
    let rejectRun: (reason: Error) => void = () => undefined;
    runBenchmark.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectRun = reject; }));
    cancelBenchmark.mockImplementationOnce(async () => {
      rejectRun(new Error("Benchmark cancelled"));
      return true;
    });
    render(<BenchmarkPage baseUrl="http://127.0.0.1:1234" model={model} authEnabled={true} />);
    fireEvent.click(screen.getByRole("button", { name: "开始基准测试" }));
    fireEvent.click(await screen.findByRole("button", { name: "取消" }));

    await screen.findByText(/测试已取消/);
    expect(cancelBenchmark).toHaveBeenCalledOnce();
    expect(screen.getByLabelText("Bearer Token")).toBeInTheDocument();
  });

  it("reports API errors without stopping the remaining runs", async () => {
    runBenchmark.mockRejectedValueOnce(new Error("API returned 401 Unauthorized: authentication failed"));
    render(<BenchmarkPage baseUrl="http://127.0.0.1:1234" model={model} authEnabled={false} />);
    fireEvent.change(screen.getByLabelText("运行次数"), { target: { value: "2" } });
    fireEvent.click(screen.getByRole("button", { name: "开始基准测试" }));

    await screen.findByText(/authentication failed/);
    await waitFor(() => expect(runBenchmark).toHaveBeenCalledTimes(2));
    expect(screen.getByText("1", { selector: ".metric-grid b" })).toBeInTheDocument();
  });

  it("disables runs when no model is active", () => {
    render(<BenchmarkPage baseUrl="http://127.0.0.1:1234" authEnabled={false} />);
    expect(screen.getByText("没有正在运行的模型")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "开始基准测试" })).toBeDisabled();
  });
});
