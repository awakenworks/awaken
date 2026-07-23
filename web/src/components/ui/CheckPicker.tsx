import {
  CheckPicker as SharedCheckPicker,
  type CheckPickerOption,
  type CheckPickerProps as SharedCheckPickerProps,
} from "@awaken/ui";

export type CheckOption = CheckPickerOption;
export type CheckPickerProps = Omit<SharedCheckPickerProps, "classes">;

/** Awaken class/token adapter over the shared controlled multi-select. */
export function CheckPicker(props: CheckPickerProps) {
  return (
    <SharedCheckPicker
      {...props}
      classes={{
        root: "check-picker",
        row: "check-row",
        label: "mono",
        description: "mut check-desc",
        empty: "mut",
      }}
    />
  );
}
