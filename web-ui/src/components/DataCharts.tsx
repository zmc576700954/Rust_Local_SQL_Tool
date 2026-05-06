import React, { useState, useMemo } from 'react';
import { BarChart, Bar, LineChart, Line, PieChart, Pie, Cell, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import { BarChart2, LineChart as LineChartIcon, PieChart as PieChartIcon } from 'lucide-react';

interface DataChartsProps {
  data: any[];
}

const COLORS = ['#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6', '#ec4899', '#06b6d4', '#14b8a6'];

export function DataCharts({ data }: DataChartsProps) {
  const [chartType, setChartType] = useState<'bar' | 'line' | 'pie'>('bar');
  const [xAxis, setXAxis] = useState<string>('');
  const [yAxis, setYAxis] = useState<string>('');

  const columns = useMemo(() => {
    if (!data || data.length === 0) return [];
    return Object.keys(data[0]);
  }, [data]);

  // Auto-select initial axes
  React.useEffect(() => {
    if (columns.length > 0) {
      if (!xAxis) setXAxis(columns[0]);
      if (!yAxis && columns.length > 1) {
        // Try to find a numeric column for Y axis
        const numericCol = columns.find(col => typeof data[0][col] === 'number');
        setYAxis(numericCol || columns[1]);
      } else if (!yAxis) {
        setYAxis(columns[0]);
      }
    }
  }, [columns, data, xAxis, yAxis]);

  const chartData = useMemo(() => {
    if (!xAxis || !yAxis || !data) return [];
    return data.slice(0, 1000).map(row => ({
      ...row,
      [xAxis]: String(row[xAxis]), // Ensure X axis is categorical/string
      [yAxis]: Number(String(row[yAxis]).replace(/,/g, '')) || 0 // Ensure Y axis is numeric
    }));
  }, [data, xAxis, yAxis]);

  if (!data || data.length === 0) {
    return <div className="flex items-center justify-center h-full text-gray-500">No data available for charts</div>;
  }

  return (
    <div className="flex flex-col h-full bg-[#0a0c10] text-gray-300">
      <div className="flex items-center gap-4 p-4 border-b border-[#30363d] bg-[#161b22] shrink-0">
        <div className="flex items-center gap-2 bg-[#0d1117] p-1 rounded-lg border border-[#30363d]">
          <button
            onClick={() => setChartType('bar')}
            className={`p-1.5 rounded transition-colors ${chartType === 'bar' ? 'bg-blue-500/20 text-blue-400' : 'hover:bg-[#21262d] text-gray-400'}`}
            title="Bar Chart"
          >
            <BarChart2 className="w-4 h-4" />
          </button>
          <button
            onClick={() => setChartType('line')}
            className={`p-1.5 rounded transition-colors ${chartType === 'line' ? 'bg-blue-500/20 text-blue-400' : 'hover:bg-[#21262d] text-gray-400'}`}
            title="Line Chart"
          >
            <LineChartIcon className="w-4 h-4" />
          </button>
          <button
            onClick={() => setChartType('pie')}
            className={`p-1.5 rounded transition-colors ${chartType === 'pie' ? 'bg-blue-500/20 text-blue-400' : 'hover:bg-[#21262d] text-gray-400'}`}
            title="Pie Chart"
          >
            <PieChartIcon className="w-4 h-4" />
          </button>
        </div>

        {data.length > 1000 && (
          <div className="text-xs text-yellow-500 flex items-center ml-2">
            ⚠️ Showing first 1000 rows only to preserve browser performance.
          </div>
        )}

        <div className="h-6 w-px bg-[#30363d]"></div>

        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-400 font-medium">X-Axis:</label>
          <select
            value={xAxis}
            onChange={(e) => setXAxis(e.target.value)}
            className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-blue-500"
          >
            {columns.map(col => (
              <option key={col} value={col}>{col}</option>
            ))}
          </select>
        </div>

        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-400 font-medium">Y-Axis:</label>
          <select
            value={yAxis}
            onChange={(e) => setYAxis(e.target.value)}
            className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-blue-500"
          >
            {columns.map(col => (
              <option key={col} value={col}>{col}</option>
            ))}
          </select>
        </div>
      </div>

      <div className="flex-1 p-4 min-h-[300px]">
        <ResponsiveContainer width="100%" height="100%">
          {chartType === 'bar' ? (
            <BarChart data={chartData} margin={{ top: 20, right: 30, left: 20, bottom: 20 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="#30363d" vertical={false} />
              <XAxis dataKey={xAxis} stroke="#8b949e" tick={{ fill: '#8b949e', fontSize: 12 }} />
              <YAxis stroke="#8b949e" tick={{ fill: '#8b949e', fontSize: 12 }} />
              <Tooltip 
                contentStyle={{ backgroundColor: '#161b22', borderColor: '#30363d', color: '#c9d1d9' }}
                itemStyle={{ color: '#c9d1d9' }}
              />
              <Legend wrapperStyle={{ fontSize: 12, color: '#8b949e' }} />
              <Bar dataKey={yAxis} fill="#3b82f6" radius={[4, 4, 0, 0]} />
            </BarChart>
          ) : chartType === 'line' ? (
            <LineChart data={chartData} margin={{ top: 20, right: 30, left: 20, bottom: 20 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="#30363d" vertical={false} />
              <XAxis dataKey={xAxis} stroke="#8b949e" tick={{ fill: '#8b949e', fontSize: 12 }} />
              <YAxis stroke="#8b949e" tick={{ fill: '#8b949e', fontSize: 12 }} />
              <Tooltip 
                contentStyle={{ backgroundColor: '#161b22', borderColor: '#30363d', color: '#c9d1d9' }}
                itemStyle={{ color: '#c9d1d9' }}
              />
              <Legend wrapperStyle={{ fontSize: 12, color: '#8b949e' }} />
              <Line type="monotone" dataKey={yAxis} stroke="#10b981" strokeWidth={2} dot={{ r: 4, fill: '#10b981', strokeWidth: 0 }} activeDot={{ r: 6 }} />
            </LineChart>
          ) : (
            <PieChart margin={{ top: 20, right: 30, left: 20, bottom: 20 }}>
              <Pie
                data={chartData}
                dataKey={yAxis}
                nameKey={xAxis}
                cx="50%"
                cy="50%"
                outerRadius={100}
                label={({ name, percent }) => `${name} ${((percent || 0) * 100).toFixed(0)}%`}
                labelLine={{ stroke: '#8b949e' }}
              >
                {chartData.map((_entry, index) => (
                  <Cell key={`cell-${index}`} fill={COLORS[index % COLORS.length]} />
                ))}
              </Pie>
              <Tooltip 
                contentStyle={{ backgroundColor: '#161b22', borderColor: '#30363d', color: '#c9d1d9' }}
                itemStyle={{ color: '#c9d1d9' }}
              />
              <Legend wrapperStyle={{ fontSize: 12, color: '#8b949e' }} />
            </PieChart>
          )}
        </ResponsiveContainer>
      </div>
    </div>
  );
}
