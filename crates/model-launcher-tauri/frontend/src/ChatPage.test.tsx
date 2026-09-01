import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import ChatPage from "./ChatPage";

afterEach(cleanup);

describe("ChatPage", () => {
  it("sends the conversation and displays the assistant reply", async () => {
    const complete = vi.fn().mockResolvedValue("Hello from the local model");
    render(<ChatPage baseUrl="http://127.0.0.1:1234" model="qwen" authenticationEnabled={false} complete={complete} />);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Hello" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(await screen.findByText("Hello from the local model")).toBeInTheDocument();
    expect(complete).toHaveBeenCalledWith({ model: "qwen", messages: [{ role: "user", content: "Hello" }], token: undefined });
  });

  it("shows loading and an unavailable-server error", async () => {
    let reject!: (reason: unknown) => void;
    const complete = vi.fn().mockReturnValue(new Promise((_, rejectRequest) => { reject = rejectRequest; }));
    render(<ChatPage baseUrl="http://127.0.0.1:1234" model="qwen" authenticationEnabled={false} complete={complete} />);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Hello" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(screen.getByLabelText("Assistant is responding")).toBeInTheDocument();
    reject("Could not reach the local API at http://127.0.0.1:1234.");
    expect(await screen.findByRole("alert")).toHaveTextContent("Could not reach the local API");
    await waitFor(() => expect(screen.queryByLabelText("Assistant is responding")).not.toBeInTheDocument());
  });

  it("does not send when no model is running", () => {
    const complete = vi.fn();
    render(<ChatPage baseUrl="http://127.0.0.1:1234" authenticationEnabled={false} complete={complete} />);
    expect(screen.getByText("No model is running")).toBeInTheDocument();
    expect(complete).not.toHaveBeenCalled();
  });

  it("uses the supplied token when authentication is enabled", async () => {
    const complete = vi.fn().mockResolvedValue("Authenticated");
    render(<ChatPage baseUrl="http://127.0.0.1:1234" model="qwen" authenticationEnabled complete={complete} />);
    fireEvent.change(screen.getByPlaceholderText("Paste a Bearer token"), { target: { value: "secret" } });
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Hello" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await screen.findByText("Authenticated");
    expect(complete).toHaveBeenCalledWith(expect.objectContaining({ token: "secret" }));
  });
});
