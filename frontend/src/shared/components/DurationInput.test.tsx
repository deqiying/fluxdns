import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { expect, it } from "vitest";
import { App } from "antd";
import { DurationInput } from "./DurationInput";

function Controlled({ initial }: { initial: string }) {
  const [value, setValue] = useState(initial);
  return (
    <App>
      <DurationInput label="主要超时" value={value} onChange={(next) => setValue(next ?? "")} />
      <output data-testid="emitted">{value}</output>
    </App>
  );
}

function unitOf(label: string) {
  return screen.getByLabelText(`${label}单位`).closest(".ant-select")?.textContent;
}

it("键入过程中不被回显归一化打断单位", async () => {
  const user = userEvent.setup();
  render(<Controlled initial="5s" />);
  const amount = screen.getByLabelText("主要超时");
  expect(amount).toHaveValue("5");

  await user.clear(amount);
  await user.type(amount, "1.5");
  // 1.5 秒换算成毫秒也成立，但控件必须保留用户选择的秒，不能中途改成毫秒。
  expect(amount).toHaveValue("1.5");
  expect(unitOf("主要超时")).toBe("秒");
  expect(screen.getByTestId("emitted")).toHaveTextContent("1.5s");
});

it("外部值变化时重新换算数值与单位", () => {
  const { rerender } = render(<App><DurationInput label="主要超时" value="5s" /></App>);
  expect(screen.getByLabelText("主要超时")).toHaveValue("5");
  expect(unitOf("主要超时")).toBe("秒");

  rerender(<App><DurationInput label="主要超时" value="1500000000ns" /></App>);
  expect(screen.getByLabelText("主要超时")).toHaveValue("1500");
  expect(unitOf("主要超时")).toBe("毫秒");

  rerender(<App><DurationInput label="主要超时" value="3000000000ns" /></App>);
  expect(screen.getByLabelText("主要超时")).toHaveValue("3");
  expect(unitOf("主要超时")).toBe("秒");

  rerender(<App><DurationInput label="主要超时" value={undefined} /></App>);
  expect(screen.getByLabelText("主要超时")).toHaveValue("");
  expect(unitOf("主要超时")).toBe("秒");
});
