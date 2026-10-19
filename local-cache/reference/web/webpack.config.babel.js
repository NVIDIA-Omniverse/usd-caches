// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

const path = require('path');

resolve: {
    modulesDirectories: ['./src', './node_modules']
};

module.exports = {
    target: 'web',
    entry: './src/main.js',
    module: {
        rules: [
            {
                test: /\.(?:js|jsx)$/,
                exclude: /node_modules/,
                use: {
                    loader: 'babel-loader',
                    options: {
                        targets: 'defaults',
                        presets: [
                            ['@babel/preset-env'],
                            ['@babel/preset-react'],
                        ],
                    },
                }
            },
            {
                test: /\.(?:ts|tsx)$/,
                exclude: /node_modules/,
                use: {
                    loader: 'ts-loader',
                }
            },
        ],
    },
    //stats: {
    //    loggingDebug: ['babel-loader'],
    //},
    resolve: {
        extensions: ['', '.js', '.jsx', '.ts', '.tsx'],
    },
    output: {
        filename: 'bundle.js',
        path: path.resolve(__dirname, 'dist'),
    },
};
