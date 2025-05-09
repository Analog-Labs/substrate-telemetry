// Source code for the Substrate Telemetry Server.
// Copyright (C) 2023 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//Column for specifying which kernel each node is running on
import * as React from 'react';
import { ColumnProps } from './';
import { Node } from '../../../state';
import icon from '../../../icons/file-binary.svg';

export class LinuxKernelColumn extends React.Component<ColumnProps> {
  public static readonly label = 'Linux Kernel';
  public static readonly icon = icon;
  public static readonly width = 154;
  public static readonly setting = 'linux_kernel';
  public static readonly sortBy = ({ linux_kernel }: Node) => linux_kernel || 0;

  private data: string;

  public shouldComponentUpdate(nextProps: ColumnProps) {
    return this.data !== nextProps.node.linux_kernel;
  }

  render() {
    const { linux_kernel } = this.props.node;

    this.data = linux_kernel;

    return <td className="Column">{linux_kernel || '-'}</td>;
  }
}
