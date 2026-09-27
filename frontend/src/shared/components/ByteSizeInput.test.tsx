import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { expect, it } from "vitest";
import { App, Button, Form } from "antd";
import { ByteSizeInput } from "./ByteSizeInput";

function Controlled({ initial }: { initial: number | undefined }) {
  const [value, setValue] = useState(initial);
  return (
    <App>
      <ByteSizeInput label="内存上限" value={value} onChange={setValue} />
      <output data-testid="emitted">{value === undefined ? "未设置" : value}</output>
    </App>
  );
}

/**
 * 用带 required 规则的真实 Form.Item 承载控件：
 * 失效输入必须让 validateFields() reject，界面与提交值不能各自为政。
 */
function RequiredFormHarness({ initial }: { initial: number }) {
  const [form] = Form.useForm<{ cache_size_bytes?: number }>();
  const [checked, setChecked] = useState("未校验");
  const validate = () => {
    form.validateFields().then(
      (values) => setChecked(`通过 ${values.cache_size_bytes}`),
      () => setChecked(`拒绝 ${String(form.getFieldValue("cache_size_bytes"))}`),
    );
  };
  return (
    <App>
      <Form form={form} initialValues={{ cache_size_bytes: initial }}>
        <Form.Item name="cache_size_bytes" label="内存上限" rules={[{ required: true }]}>
          <ByteSizeInput label="内存上限" />
        </Form.Item>
      </Form>
      <Button onClick={validate}>触发校验</Button>
      <output data-testid="checked">{checked}</output>
    </App>
  );
}

function unitOf(label: string) {
  return screen.getByLabelText(`${label}单位`).closest(".ant-select")?.textContent;
}

it("回显时挑选能精确表示的最大单位", () => {
  const { rerender } = render(<App><ByteSizeInput label="内存上限" value={67_108_864} /></App>);
  // 数值 ≥ 1 优先：64 MiB 回显 64 MB，而不是数值不足 1 的 0.0625 GB。
  expect(screen.getByLabelText("内存上限")).toHaveValue("64");
  expect(unitOf("内存上限")).toBe("MB");

  // 小数回显无损且数值 ≥ 1：768 MiB 用 768 MB 表示，而不是 0.75 GB。
  rerender(<App><ByteSizeInput label="内存上限" value={805_306_368} /></App>);
  expect(screen.getByLabelText("内存上限")).toHaveValue("768");
  expect(unitOf("内存上限")).toBe("MB");

  // 超过 2 位小数但不超过 6 位：807306368 B 退到 KB 而不是整数字节。
  rerender(<App><ByteSizeInput label="内存上限" value={807_306_368} /></App>);
  expect(screen.getByLabelText("内存上限")).toHaveValue("788385.125");
  expect(unitOf("内存上限")).toBe("KB");

  rerender(<App><ByteSizeInput label="内存上限" value={1_073_741_824} /></App>);
  expect(screen.getByLabelText("内存上限")).toHaveValue("1");
  expect(unitOf("内存上限")).toBe("GB");

  // 1 TiB 回显为 1 TB，而不是 1024 GB。
  rerender(<App><ByteSizeInput label="内存上限" value={1_099_511_627_776} /></App>);
  expect(screen.getByLabelText("内存上限")).toHaveValue("1");
  expect(unitOf("内存上限")).toBe("TB");
});

it("键入小数时按 1024 进制换算成整数字节", async () => {
  const user = userEvent.setup();
  render(<Controlled initial={1_048_576} />);
  const amount = screen.getByLabelText("内存上限");
  expect(amount).toHaveValue("1");
  expect(unitOf("内存上限")).toBe("MB");

  await user.clear(amount);
  await user.type(amount, "1.5");
  // 1.5 MB = 1_572_864 B；键入过程中的小数不会被回显归一化打断单位。
  expect(amount).toHaveValue("1.5");
  expect(unitOf("内存上限")).toBe("MB");
  expect(screen.getByTestId("emitted")).toHaveTextContent("1572864");
});

it("切换单位后按新单位换算回整数字节", async () => {
  const user = userEvent.setup();
  render(<Controlled initial={3_145_728} />);
  expect(screen.getByLabelText("内存上限")).toHaveValue("3");
  expect(unitOf("内存上限")).toBe("MB");
  expect(screen.getByTestId("emitted")).toHaveTextContent("3145728");

  // 切换单位只改换算口径：数字保留，字节数按新单位重算。
  await user.click(screen.getByLabelText("内存上限单位"));
  await user.click(await screen.findByTitle("KB"));
  expect(unitOf("内存上限")).toBe("KB");
  expect(screen.getByLabelText("内存上限")).toHaveValue("3");
  expect(screen.getByTestId("emitted")).toHaveTextContent("3072");

  await user.click(screen.getByLabelText("内存上限单位"));
  await user.click(await screen.findByTitle("GB"));
  expect(unitOf("内存上限")).toBe("GB");
  expect(screen.getByLabelText("内存上限")).toHaveValue("3");
  expect(screen.getByTestId("emitted")).toHaveTextContent("3221225472");
});

it("超出上限的输入 emit undefined 而不是保留旧值", async () => {
  const user = userEvent.setup();
  render(<Controlled initial={1_073_741_824} />);
  const amount = screen.getByLabelText("内存上限");
  expect(unitOf("内存上限")).toBe("GB");

  // 1500 GB 超过 Bytes 上限（1024 GB）：必须显式置为未设置，绝不能静默留下 150 GB 这类旧值。
  await user.type(amount, "500");
  expect(amount).toHaveValue("1500");
  expect(unitOf("内存上限")).toBe("GB");
  expect(screen.getByTestId("emitted")).toHaveTextContent("未设置");
});

it("超出上限后在带 required 规则的 Form.Item 中校验失败", async () => {
  const user = userEvent.setup();
  render(<RequiredFormHarness initial={1_073_741_824} />);
  const amount = screen.getByLabelText("内存上限");
  expect(amount).toHaveValue("1");

  await user.type(amount, "500");
  await user.click(screen.getByRole("button", { name: "触发校验" }));
  // undefined 被 required 拦下：表单值也确实是 undefined，不会再提交上一个有效字节数。
  expect(await screen.findByTestId("checked")).toHaveTextContent("拒绝 undefined");
});

it("清空输入后在带 required 规则的 Form.Item 中校验失败", async () => {
  const user = userEvent.setup();
  render(<RequiredFormHarness initial={1_073_741_824} />);
  const amount = screen.getByLabelText("内存上限");

  await user.clear(amount);
  expect(amount).toHaveValue("");
  await user.click(screen.getByRole("button", { name: "触发校验" }));
  expect(await screen.findByTestId("checked")).toHaveTextContent("拒绝 undefined");
});
