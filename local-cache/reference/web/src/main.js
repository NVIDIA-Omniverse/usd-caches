// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import React from 'react';
import ReactDOM from 'react-dom';
import { createRoot } from 'react-dom/client';

////////////////////////////////////////////////////////////////////////////////
// Utility
////////////////////////////////////////////////////////////////////////////////

function formatBytes(bytes, decimals = 2) {
    if (!+bytes) return '0 Bytes'

    const k = 1024
    const dm = decimals < 0 ? 0 : decimals
    const sizes = ['Bytes', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB', 'ZiB', 'YiB']

    const i = Math.floor(Math.log(bytes) / Math.log(k))

    return [`${parseFloat((bytes / Math.pow(k, i)).toFixed(dm))}`, `${sizes[i]}`]
}

////////////////////////////////////////////////////////////////////////////////
// Hub
////////////////////////////////////////////////////////////////////////////////

const params = new Proxy(new URLSearchParams(window.location), {
    get: (sParams, prop) => sParams.get(prop),
});

if (params.port == null) {
    document.getElementById("cache_cleanup_threshold").disabled = true;
    document.getElementById("cache_garbage_collection_interval").disabled = true;
    document.getElementById("cache_max_size").disabled = true;
    document.getElementById("cache_path").disabled = true;
    document.getElementById("clear_cache").disabled = true;
    document.getElementById("submit").disabled = true;

    alert('Hub port not found');
    throw new Error('Hub port not found');
}

const form = document.getElementById("post");
const feedback = document.getElementById("feedback")
const baseEndpoint = `http://localhost:${params.port}/api/1.0`;
const contentType = { 'Content-type': 'application/json; charset=UTF-8' };

window.addEventListener('load', loadData);
form.addEventListener('submit', saveConfig);
document.getElementById("clear_cache").addEventListener("click", clearCache, false);
document.getElementById("support_bundle").addEventListener("click", downloadSupportBundle, false);

async function saveConfig(e) {
    e.preventDefault();
    if (params.port == null) return;

    const data = new FormData(form);

    const payload = {
        "cache": {
            "path": data.get('cache_path'),
            "max_size": `${parseInt(data.get('cache_max_size'), 10)}000000000`,
            "cleanup_threshold": parseInt(data.get('cache_cleanup_threshold'), 10),
            "garbage_collection_interval": parseInt(data.get('cache_garbage_collection_interval'), 10)
        }
    };

    try {
        await fetch(`${baseEndpoint}/service/config`, {
            method: 'PUT',
            body: JSON.stringify(payload),
            headers: contentType
        });

        setSuccess("Updated Successfully");
    } catch (e) {
        setError("Something went wrong. Error" + e.toString());
    }
}

function setSuccess(msg) {
    feedback.innerHTML = msg;
    feedback.classList.add("feedback-success");
    feedback.classList.remove("hide");


    setTimeout(function () {
        feedback.classList.add("hide");
        feedback.classList.remove("feedback-success");
        feedback.innerHTML = "";
    }, 3000)
}

function setError(msg) {
    feedback.innerHTML = msg;
    feedback.classList.add("feedback-error");
    feedback.classList.remove("hide");

    setTimeout(function () {
        feedback.classList.add("hide");
        feedback.classList.remove("feedback-error");
        feedback.innerHTML = "";
    }, 3000)
}

async function loadData() {
    await Promise
        .all([
            loadConfig(),
            loadCacheStats()
        ])
        .then(console.log('Data loaded'));
}

async function loadConfig() {
    if (params == null) {
        alert('Hub Port not found');
        return;
    }

    console.log('Load config');
    // Fetch data
    return fetch(`${baseEndpoint}/service/config`, {
        headers: contentType
    })
        .then((response) => response.json())
        .then(populateForm);
}

async function loadCacheStats() {
    if (params == null) {
        alert('Hub Port not found');
        return;
    }

    console.log('Load cache stats');
    // Fetch data
    return fetch(`${baseEndpoint}/cache/stats`, {
        headers: contentType
    })
        .then((response) => response.json())
        .then(populateCacheStats);
}

function getCacheSize() {
    return document.getElementById("cache_size").innerText;
}

function populateCacheStats(data) {
    if(data.hasOwnProperty('cache_size')) {
        let info = formatBytes(data['cache_size'])
        document.getElementById("cache_size").innerText = info[0];
        document.getElementById("cache_size_info").innerText = info[1];
    }

    if(data.hasOwnProperty('cache_entries')) {
        document.getElementById("cache_files").innerText = data['cache_entries'];
    }
}

function populateForm(data) {
    let path = data['cache']['path'];
    let max_size = parseMaxSize(data);
    let cleanup_threshold = parseInt(data['cache']['cleanup_threshold'], 10);
    let garbage_collection_interval = parseInt(data['cache']['garbage_collection_interval'], 10);

    document.getElementById("cache_cleanup_threshold").value = cleanup_threshold;
    document.getElementById("cache_garbage_collection_interval").value = garbage_collection_interval;
    document.getElementById("cache_max_size").value = max_size;
    document.getElementById("cache_path").value = path;
}

function parseMaxSize(data) {
    return parseInt(data['cache']['max_size'].split(' ')[0]);
}

async function clearCache() {
    if (params.port == null) {
        alert('Hub Port not found');
        return;
    }

    let res = await fetch(
        `${baseEndpoint}/cache/clear`,
        {
            method: 'DELETE',
            headers: contentType
        }
    );

    if (res.status === 200) {
        setSuccess("Cache cleared successfully.");
    } else {
        setError("Cache not cleared.");
    }
    await loadCacheStats();
}

export function downloadSupportBundle() {
    if (params.port == null) {
        alert('Hub Port not found');
        return;
    }
    window.location.assign(`http://localhost:${params.port}/api/1.0/service/supportbundle`);
}

////////////////////////////////////////////////////////////////////////////////
// Page Selection
////////////////////////////////////////////////////////////////////////////////

function selectHub() {
    document.getElementById("hub_page").style.display = 'block';
}

document.getElementById('hub_selector').onclick = selectHub; 
window.addEventListener('load', selectHub);
